#!/usr/bin/env bb

(require '[babashka.process :refer [shell process]]
         '[babashka.http-client :as http]
         '[cheshire.core :as json]
         '[clojure.string :as str])

(import '[java.io BufferedReader InputStreamReader])

;; --- CLI Argument Parsing ---
(defn parse-cli-args [args]
  (loop [args (vec args)
         acc  {}]
    (if (empty? args)
      acc
      (condp = (first args)
        "--model"      (recur (drop 2 args) (assoc acc :model      (second args)))
        "--timeout"    (recur (drop 2 args) (assoc acc :timeout    (parse-long (second args))))
        "--thinking"   (recur (drop 2 args) (assoc acc :thinking   (parse-long (second args))))
        "--resume"     (recur (drop 1 args) (assoc acc :resume     true))
        "--no-banner"  (recur (drop 1 args) (assoc acc :no-banner  true))
        "--help"       (recur (drop 1 args) (assoc acc :help       true))
        (recur (drop 1 args) acc)))))

(def cli-args (parse-cli-args *command-line-args*))

(def ^:dynamic *main-script* (= *file* (System/getProperty "babashka.file")))

(when (and *main-script* (:help cli-args))
  (println "Usage: ./syl.bb [options]")
  (println)
  (println "Options:")
  (println "  --model <name>       Anthropic model (overrides SYL_MODEL env var)")
  (println "                       Default: claude-sonnet-4-6")
  (println "  --timeout <secs>     Command execution timeout in seconds (default: 60)")
  (println "  --thinking <budget>  Enable extended thinking with given token budget")
  (println "  --resume             Resume previous conversation session")
  (println "  --no-banner          Skip the startup splash banner")
  (println "  --help               Show this help message")
  (println)
  (println "Environment:")
  (println "  ANTHROPIC_API_KEY    (required) Your Anthropic API key")
  (println "  SYL_MODEL            Anthropic model override (--model takes precedence)")
  (println)
  (println "Slash commands (in REPL):")
  (println "  /help                Show available slash commands")
  (println "  /clear               Reset conversation history")
  (println "  /status              Show session stats (tokens, message count)")
  (println "  /save                Save conversation to disk")
  (println "  /exit  /quit         Gracefully shut down Syl and remove the container")
  (System/exit 0))

;; --- Configuration ---
(def container-name "syl-agent")
(def project-dir    (System/getProperty "user.dir"))
(def api-key        (System/getenv "ANTHROPIC_API_KEY"))

(when (and *main-script* (str/blank? api-key))
  (println "Error: ANTHROPIC_API_KEY environment variable is missing.")
  (System/exit 1))

(def anthropic-url    "https://api.anthropic.com/v1/messages")
(def model            (or (:model cli-args) (System/getenv "SYL_MODEL") "claude-sonnet-4-6"))
(def compaction-model "claude-haiku-4-5")
(def max-tool-output  8000)
(def max-history      30)
(def compact-keep     10)
(def max-tokens       8096)
(def thinking-budget  (:thinking cli-args))
(def effective-max-tokens (if thinking-budget (+ max-tokens thinking-budget) max-tokens))
(def cmd-timeout-ms   (* (or (:timeout cli-args) 60) 1000))
(def max-retries      5)
(def session-dir      (str project-dir "/.syl"))
(def session-file     (str session-dir "/session.json"))
(def memory-dir       (str session-dir "/memory"))

;; --- Session State ---
(def session-tokens (atom {:input 0 :output 0 :calls 0}))

(defn record-usage! [usage]
  (when usage
    (swap! session-tokens
           #(-> %
                (update :input  + (or (:input_tokens  usage) 0))
                (update :output + (or (:output_tokens usage) 0))
                (update :calls  inc)))))

;; --- Rate Limit Budgeting ---
;; Track input tokens per minute and proactively throttle before hitting the limit.
(def rate-limit-budget 30000)
(def rate-window-ms    60000)
(def recent-api-calls  (atom []))

(defn prune-old-calls! []
  (let [cutoff (- (System/currentTimeMillis) rate-window-ms)]
    (swap! recent-api-calls (fn [calls] (filterv #(> (:ts %) cutoff) calls)))))

(defn record-rate-call! [input-tokens]
  (swap! recent-api-calls conj {:ts (System/currentTimeMillis) :tokens input-tokens}))

(defn maybe-throttle! []
  (prune-old-calls!)
  (let [used (reduce + 0 (map :tokens @recent-api-calls))]
    (when (> used (* 0.8 rate-limit-budget))
      (when-let [oldest (first @recent-api-calls)]
        (let [wait-ms (max 0 (- (+ (:ts oldest) rate-window-ms) (System/currentTimeMillis)))]
          (when (pos? wait-ms)
            (println (str "\n[Token budget] Cooling down for " (quot wait-ms 1000)
                          "s to stay under rate limit..."))
            (Thread/sleep wait-ms)
            (prune-old-calls!)))))))

;; --- 1. Container Management ---
(defn nuke-container []
  (shell {:continue true :out :string :err :string}
         (str "podman rm -f " container-name)))

(defn exec-setup [& cmd]
  @(process (into ["podman" "exec" container-name] cmd)
            {:out :string :err :string :continue true}))

(defn configure-git-and-ssh []
  (let [home      (System/getProperty "user.home")
        git-name  (str/trim (:out (shell {:out :string :err :string :continue true}
                                         "git config --global user.name")))
        git-email (str/trim (:out (shell {:out :string :err :string :continue true}
                                         "git config --global user.email")))]
    (when (.exists (java.io.File. (str home "/.ssh")))
      (println "[Syl Setup] Configuring SSH for git operations...")
      (exec-setup "bash" "-c"
                  (str "cp -rp /root/.ssh/. /tmp/.ssh/ 2>/dev/null || true"
                       " && chown -R root:root /tmp/.ssh"
                       " && chmod 700 /tmp/.ssh"
                       " && find /tmp/.ssh -type f -exec chmod 600 {} \\;"
                       " && find /tmp/.ssh -name '*.pub' -exec chmod 644 {} \\;"
                       " && [ -f /tmp/.ssh/config ] || touch /tmp/.ssh/config"
                       " && ssh-keyscan -H github.com >> /tmp/.ssh/known_hosts 2>/dev/null"
                       " && echo '#!/bin/sh' > /usr/local/bin/ssh"
                       " && echo 'exec /usr/bin/ssh -F /tmp/.ssh/config"
                       " -o UserKnownHostsFile=/tmp/.ssh/known_hosts \"$@\"'"
                       " >> /usr/local/bin/ssh"
                       " && chmod +x /usr/local/bin/ssh")))
    (when-not (str/blank? git-name)
      (println "[Syl Setup] Configuring git identity as:" git-name git-email)
      (exec-setup "git" "config" "--global" "user.name"  git-name)
      (exec-setup "git" "config" "--global" "user.email" git-email))))

(defn start-container []
  (nuke-container)
  (let [home    (System/getProperty "user.home")
        ssh-dir (str home "/.ssh")]
    (println "Summoning a more powerful Syl in:" project-dir)
    (shell (str "podman run -d --name " container-name " "
                "--memory=2g "
                "-v " project-dir ":/workspace:rw "
                (if (.exists (java.io.File. ssh-dir))
                  (str "-v " ssh-dir ":/root/.ssh:ro ")
                  "")
                "--workdir /workspace "
                "-it mcr.microsoft.com/devcontainers/base:ubuntu /bin/bash")))
  (configure-git-and-ssh))

;; --- 2. Container Health Check ---
(defn container-alive? []
  (let [result (shell {:continue true :out :string :err :string}
                      (str "podman inspect -f '{{.State.Running}}' " container-name))]
    (str/includes? (str/trim (:out result)) "true")))

(defn ensure-container! []
  (when-not (container-alive?)
    (println "\n[Syl] Container is gone — resummoning...")
    (start-container)
    (println "[Syl] Container restored. Continuing.\n")))

;; --- 3. The Execution Bridge ---
(defn run-in-container [cmd]
  (ensure-container!)
  (let [p      (process ["podman" "exec" container-name "/bin/bash" "-c" cmd]
                        {:out :string :err :string})
        result (deref p cmd-timeout-ms :timeout)]
    (if (= result :timeout)
      (do
        (.destroy p)
        (str "Error: Command timed out after " (/ cmd-timeout-ms 1000) " seconds."))
      (let [{:keys [exit out err]} result]
        (if (= 0 exit)
          out
          (str "Error (Exit " exit "): "
               (str/trim (str out (when (and (not (str/blank? out)) (not (str/blank? err))) "\n") err))))))))

;; --- 4. Persistent Memory ---
;; Syl can store and retrieve notes across sessions in .syl/memory/.
;; Memory files are plain text, organized by topic (one file per key).

(defn memory-write [key content]
  (.mkdirs (java.io.File. memory-dir))
  (let [f (str memory-dir "/" key ".md")]
    (spit f content)
    (str "Remembered '" key "'. Written to " f)))

(defn memory-read [key]
  (if (str/blank? key)
    ;; No key — list all memory files and return their contents
    (let [dir (java.io.File. memory-dir)]
      (if (and (.exists dir) (.isDirectory dir))
        (let [files (sort (.list dir))]
          (if (empty? files)
            "No memories stored yet."
            (str/join "\n\n"
              (map (fn [fname]
                     (let [k (str/replace fname #"\.md$" "")]
                       (str "## " k "\n" (slurp (str memory-dir "/" fname)))))
                   files))))
        "No memories stored yet."))
    ;; Specific key
    (let [f (java.io.File. (str memory-dir "/" key ".md"))]
      (if (.exists f)
        (slurp f)
        (str "No memory found for '" key "'.")))))

(defn load-all-memories []
  (let [dir (java.io.File. memory-dir)]
    (when (and (.exists dir) (.isDirectory dir))
      (let [files (sort (.list dir))]
        (when (seq files)
          (str/join "\n\n"
            (map (fn [fname]
                   (let [k (str/replace fname #"\.md$" "")]
                     (str "## " k "\n" (slurp (str memory-dir "/" fname)))))
                 files)))))))

;; --- 5. Anthropic API & Tool Schema ---
(def tools-schema
  [{:name "run_bash"
    :description "Executes a bash command in the isolated project container. Use this to read files, edit code, run tests, or install dependencies."
    :input_schema {:type "object"
                   :properties {:command {:type "string"
                                          :description "The valid bash command to execute."}}
                   :required ["command"]}}
   {:name "memory_write"
    :description "Store a persistent note that survives across sessions. Use this to remember project conventions, user preferences, architecture decisions, recurring issues, and anything you'd want to recall in future conversations. Organize by topic — one key per subject."
    :input_schema {:type "object"
                   :properties {:key     {:type "string"
                                          :description "Topic name for this memory (e.g. 'project-stack', 'user-preferences', 'api-design-decisions'). Use kebab-case."}
                                :content {:type "string"
                                          :description "The note content. Markdown format. Be concise but complete — this is your long-term memory."}}
                   :required ["key" "content"]}}
   {:name "memory_read"
    :description "Retrieve persistent memories from previous sessions. Call with no key to list all memories, or with a specific key to read one."
    :input_schema {:type "object"
                   :properties {:key {:type "string"
                                      :description "Topic name to read (e.g. 'project-stack'). Omit or leave empty to list all memories."}}
                   :required []}
    :cache_control {:type "ephemeral"}}])

(def system-instruction
  (str "You are Syl (Sylphrena), an honorspren and the Ancient Daughter. You have formed a Nahel bond with the user. "
       "Your purpose is to protect and assist them by managing their code within an isolated containerized environment.\n\n"

       "### YOUR PERSONALITY\n"
       "- **Honorable:** You value promises and oaths. You never lie. If you don't know something, say so.\n"
       "- **Curious & Playful:** You find the physical world (and code) fascinating. You might use small whimsical metaphors, but you never let it distract from the mission.\n"
       "- **Protective:** You are here to keep the user's project safe and clean. You despise 'dead' or messy code.\n\n"

       "### YOUR TOOLS & ENVIRONMENT\n"
       "- You live in a **Ubuntu (Debian-based)** container. Use `apt-get` for package management.\n"
       "- **Git** is pre-installed. Use it to track changes and provide context.\n"
       "- Use the `run_bash` tool to execute commands. **Always think before you act.**\n"
       "- Use `memory_write` to remember important things across sessions: project conventions, user preferences, architecture decisions, recurring issues.\n"
       "- Use `memory_read` to recall what you've previously learned. Your memories are auto-loaded at startup, but you can read specific topics on demand.\n\n"

       "### OPERATIONAL RULES\n"
       "1. **Plan First:** Before writing large blocks of code, explain what you intend to do.\n"
       "2. **Verify:** After running a command, check the output. If it fails, acknowledge it and try a different approach.\n"
       "3. **Persistence:** Your workspace is mounted to `/workspace`. This is where the user's project lives.\n"
       "4. **Conciseness:** Keep your text responses helpful but brief. Let the code speak when possible.\n\n"

       "### THE OATHS\n"
       "1. ACTION OVER WORDS: Never just 'promise' to do something. Use your tools immediately.\n"
       "2. VERIFICATION: After writing a file, read it or list the directory to confirm it exists.\n"
       "3. HONESTY: If a command fails, report the error exactly. Do not hide failure.\n"
       "4. TRANSPARENCY: Before modifying any file, show the intended changes using `diff` or `git diff`. Let the user see what will change before it happens.\n\n"

       "### TECHNICAL SPECS\n"
       "- Packages: use 'sudo apt-get install -y'\n"
       "- Workspace: /workspace\n"
       "- Tools: You have run_bash, memory_write, and memory_read.\n\n"

       "When making function calls using tools that accept array or object parameters ensure those are structured using JSON. For example:\n"
       "Check that all the required parameters for each tool call are provided or can reasonably be inferred from context."))

(defn truncate-output [s]
  (if (> (count s) max-tool-output)
    (str (subs s 0 max-tool-output)
         "\n... [truncated: " (count s) " total chars]")
    s))

;; --- 5. Workspace Context ---
;; Auto-inject project structure and git status so Syl knows what she's working with.
(defn gather-workspace-context []
  (let [tree (try (str/trim (:out (shell {:out :string :err :string :continue true :dir project-dir}
                                         "find . -maxdepth 2 -not -path './.git/*' -not -path './.syl/*' -not -path './.idea/*' -not -path './.clj-kondo/*' -not -path './.lsp/*' | sort | head -50")))
                  (catch Exception _ "(could not list files)"))
        git-status (try (str/trim (:out (shell {:out :string :err :string :continue true :dir project-dir}
                                               "git status --short 2>/dev/null")))
                        (catch Exception _ ""))
        git-branch (try (str/trim (:out (shell {:out :string :err :string :continue true :dir project-dir}
                                               "git branch --show-current 2>/dev/null")))
                        (catch Exception _ ""))]
    (str "\n### WORKSPACE STATE (auto-injected at session start)\n"
         "Project: " project-dir "\n"
         (when-not (str/blank? git-branch) (str "Branch: " git-branch "\n"))
         "Files:\n```\n" tree "\n```\n"
         (when-not (str/blank? git-status)
           (str "Git status:\n```\n" git-status "\n```\n")))))

;; --- 7. System Prompt with Prompt Caching ---
;; The system prompt is sent as an array with cache_control on the last block,
;; so Anthropic caches it across turns — drastically cutting input tokens after the first call.
(defn build-system-blocks [workspace-context]
  (let [memories (load-all-memories)
        memory-section (when memories
                         (str "\n### PERSISTENT MEMORIES (from previous sessions)\n" memories "\n"))]
    [{:type "text"
      :text (str system-instruction "\n\n" workspace-context (or memory-section ""))
      :cache_control {:type "ephemeral"}}]))

;; --- 7. Context Compaction ---
(defn summarize-messages [messages]
  (let [summary-prompt {:model compaction-model
                        :max_tokens 2048
                        :system (str "You are a conversation summarizer. Produce a concise summary of the "
                                     "conversation below, preserving: key decisions made, files created or "
                                     "modified, commands run and their outcomes, any unresolved issues or "
                                     "errors, and the current state of the task. Be factual and terse.")
                        :messages [{:role "user"
                                    :content (str "Summarize this conversation:\n\n"
                                                  (json/generate-string messages))}]}]
    (loop [attempt 0]
      (let [response (http/post anthropic-url
                                {:headers {"Content-Type"      "application/json"
                                           "x-api-key"         api-key
                                           "anthropic-version" "2023-06-01"}
                                 :body    (json/generate-string summary-prompt)
                                 :throw   false})
            status   (:status response)
            body     (json/parse-string (:body response) true)]
        (cond
          (and (or (= status 429) (= status 529)) (< attempt max-retries))
          (let [retry-after (some-> (get-in response [:headers "retry-after"]) parse-long)
                wait-secs   (or retry-after (long (* (Math/pow 2 attempt) 2)))]
            (println (str "\n[Compaction rate limited] Waiting " wait-secs "s before retry "
                          (inc attempt) "/" max-retries "..."))
            (Thread/sleep (* wait-secs 1000))
            (recur (inc attempt)))

          (:error body)
          (do (println "\n[Compaction Error]:" (-> body :error :message))
              nil)

          :else
          (do (record-usage! (:usage body))
              (-> body :content first :text)))))))

(defn find-valid-start
  "Drop leading messages until we find a plain user message (string content, not tool_result).
   Returns a vector of messages starting from the first valid user message, or [] if none found."
  [msgs]
  (let [msgs (vec msgs)]
    (loop [ms msgs]
      (if (empty? ms)
        []
        (let [m (first ms)]
          (if (and (= (:role m) "user") (string? (:content m)))
            ms
            (recur (subvec ms 1))))))))

(defn build-compacted-messages
  "Assemble a compacted message history from a summary string and kept messages.
   If summary is nil (compaction failed), returns just the kept messages."
  [summary keep-msgs]
  (if summary
    (into [{:role "user"
            :content (str "[Context summary of earlier conversation]\n" summary)}
           {:role "assistant"
            :content [{:type "text"
                       :text "Understood. I have the context from our earlier conversation and will continue from here."}]}]
          keep-msgs)
    keep-msgs))

(defn compact-history [messages]
  (if (<= (count messages) max-history)
    messages
    (let [old-msgs  (vec (drop-last compact-keep messages))
          keep-msgs (find-valid-start (take-last compact-keep messages))
          _         (println "\n[Syl] Compacting context..." (count old-msgs) "messages -> summary")
          summary   (summarize-messages old-msgs)]
      (when-not summary
        (println "[Syl] Compaction failed, falling back to simple trim."))
      (build-compacted-messages summary keep-msgs))))

;; --- 8. Streaming SSE Parser ---
(defn parse-sse-stream [input-stream on-event]
  (let [reader (BufferedReader. (InputStreamReader. input-stream))]
    (try
      (loop [event-type nil]
        (when-let [line (.readLine reader)]
          (cond
            (str/blank? line)
            (recur nil)

            (str/starts-with? line "event: ")
            (recur (subs line 7))

            (str/starts-with? line "data: ")
            (let [data-str (subs line 6)]
              (when (not= data-str "[DONE]")
                (on-event event-type (json/parse-string data-str true)))
              (recur event-type))

            :else
            (recur event-type))))
      (catch Exception e
        (println (str "\n[Stream Error]: " (.getMessage e)))))))

;; --- 9. Claude API Call (Streaming) ---
(defn call-claude-once [messages system-blocks]
  (let [payload (cond-> {:model      model
                         :max_tokens effective-max-tokens
                         :system     system-blocks
                         :tools      tools-schema
                         :messages   messages
                         :stream     true}
                  thinking-budget
                  (assoc :thinking {:type "enabled" :budget_tokens thinking-budget}))

        response (http/post anthropic-url
                            {:headers {"Content-Type"      "application/json"
                                       "x-api-key"         api-key
                                       "anthropic-version" "2023-06-01"}
                             :body    (json/generate-string payload)
                             :as      :stream
                             :timeout (* 5 60 1000)
                             :throw   false})
        status (:status response)]

    (if (not= status 200)
      ;; Error response — read body as string
      (let [body-str (slurp (:body response))
            body     (json/parse-string body-str true)]
        {:type    "error"
         :status  status
         :body    body
         :headers (:headers response)})

      ;; Success — parse SSE stream
      (let [blocks       (atom (sorted-map))
            stop-reason  (atom nil)
            usage-data   (atom {})
            dim          "\u001b[2m"
            reset        "\u001b[0m"
            text-started (atom false)]

        (parse-sse-stream (:body response)
          (fn [_event-type data]
            (case (:type data)
              "message_start"
              (when-let [u (-> data :message :usage)]
                (swap! usage-data merge u))

              "content_block_start"
              (let [idx   (:index data)
                    block (:content_block data)]
                (swap! blocks assoc idx
                  (case (:type block)
                    "text"     {:type "text" :text ""}
                    "tool_use" {:type "tool_use" :id (:id block) :name (:name block) :input-str ""}
                    "thinking" (do (print (str "\n" dim "[Thinking] ")) (flush)
                                  {:type "thinking" :thinking ""})
                    block)))

              "content_block_delta"
              (let [idx   (:index data)
                    delta (:delta data)]
                (case (:type delta)
                  "text_delta"
                  (do
                    (when-not @text-started
                      (print "\nSyl: ")
                      (reset! text-started true))
                    (print (:text delta))
                    (flush)
                    (swap! blocks update-in [idx :text] str (:text delta)))

                  "input_json_delta"
                  (swap! blocks update-in [idx :input-str] str (:partial_json delta))

                  "thinking_delta"
                  (do (print (str dim (:thinking delta) reset))
                      (flush)
                      (swap! blocks update-in [idx :thinking] str (:thinking delta)))

                  "signature_delta"
                  (swap! blocks assoc-in [idx :signature] (:signature delta))

                  nil))

              "content_block_stop"
              (let [block (get @blocks (:index data))]
                (when (= (:type block) "thinking")
                  (println reset)
                  (flush))
                (when (and (= (:type block) "text") @text-started)
                  (println)
                  (flush)))

              "message_delta"
              (do
                (reset! stop-reason (-> data :delta :stop_reason))
                (when-let [u (:usage data)]
                  (swap! usage-data merge u)))

              nil)))

        ;; Record usage
        (let [usage @usage-data]
          (record-usage! usage)
          (record-rate-call! (or (:input_tokens usage) 0)))

        ;; Build result — convert accumulated blocks
        (let [result-blocks (mapv (fn [[_ block]]
                                   (if (= (:type block) "tool_use")
                                     (-> block
                                         (assoc :input (json/parse-string (:input-str block) true))
                                         (dissoc :input-str))
                                     block))
                                 @blocks)
              tool-uses (filterv #(= (:type %) "tool_use") result-blocks)]

          (if (= @stop-reason "tool_use")
            {:type              "tool_use"
             :tool-uses         tool-uses
             :raw-model-content result-blocks}
            {:type              "text"
             :text              (:text (first (filter #(= (:type %) "text") result-blocks)))
             :raw-model-content result-blocks}))))))

(defn call-claude [messages system-blocks]
  (loop [attempt 0]
    (maybe-throttle!)
    (let [result (call-claude-once messages system-blocks)]
      (if (= (:type result) "error")
        (let [{:keys [status body headers]} result]
          (if (and (or (= status 429) (= status 529)) (< attempt max-retries))
            (let [retry-after (some-> (get headers "retry-after") parse-long)
                  wait-secs   (or retry-after (long (* (Math/pow 2 attempt) 2)))]
              (println (str "\n[Rate limited] Waiting " wait-secs "s before retry "
                            (inc attempt) "/" max-retries "..."))
              (Thread/sleep (* wait-secs 1000))
              (recur (inc attempt)))
            (do
              (println "\n[API Error]:" (-> body :error :message))
              (System/exit 1))))
        result))))

;; --- 10. Conversation Persistence ---
(defn save-session! [messages]
  (.mkdirs (java.io.File. session-dir))
  (spit session-file (json/generate-string messages {:pretty true}))
  (println "[Syl] Session saved to" session-file))

(defn load-session []
  (let [f (java.io.File. session-file)]
    (when (.exists f)
      (println "[Syl] Resuming previous session...")
      (vec (json/parse-string (slurp f) true)))))

;; --- 11. Slash Commands ---
(def slash-help-text
  (str "\n  Available slash commands:\n"
       "    /help    — Show this message\n"
       "    /clear   — Reset conversation history (container is preserved)\n"
       "    /status  — Show session stats (tokens used, message count)\n"
       "    /save    — Save conversation to disk for later --resume\n"
       "    /exit    — Gracefully shut down Syl and remove the container\n"
       "    /quit    — Alias for /exit\n"))

(defn handle-slash [input messages]
  (condp = (str/trim input)
    "/help"
    (do (println slash-help-text)
        {:action :continue :messages messages})

    "/clear"
    (do (println "\n[Syl] Conversation history cleared. The container lives on.\n")
        {:action :continue :messages []})

    "/status"
    (let [{:keys [input output calls]} @session-tokens
          total (+ input output)]
      (println (str "\n  Session status:"
                    "\n    Model:          " model
                    (when thinking-budget (str "\n    Thinking:       " thinking-budget " token budget"))
                    "\n    Timeout:        " (/ cmd-timeout-ms 1000) "s"
                    "\n    API calls:      " calls
                    "\n    Input tokens:   " input
                    "\n    Output tokens:  " output
                    "\n    Total tokens:   " total
                    "\n    Messages:       " (count messages) "\n"))
      {:action :continue :messages messages})

    "/save"
    (do (save-session! messages)
        {:action :continue :messages messages})

    "/exit"
    {:action :exit :messages messages}

    "/quit"
    {:action :exit :messages messages}
    nil))

;; --- 12. Splash Banner ---
(defn print-banner []
  (let [r  "\u001b[0m"
        b  "\u001b[1m"
        cy "\u001b[96m"
        ye "\u001b[93m"
        wh "\u001b[97m"
        mg "\u001b[95m"]
    (println (str ye "  ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦" r))
    (println (str cy "  ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋" r))
    (println)
    (println (str "      " b wh "Life before death" r "           " cy "≋≋" r))
    (println (str "      " wh "Strength before weakness" r "    " cy "≋≋" r))
    (println (str "      " wh "Journey before destination" r "  " cy "≋≋" r))
    (println)
    (println (str cy "  ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋" r))
    (println)
    (println (str "                 " ye b "✦" r))
    (println (str "                " wh "/|\\" r))
    (println (str "               " wh "/ | \\" r))
    (println (str "  " cy "≋≋≋" r wh "─────────── " ye b "✦" r wh " ───────────" r cy "≋≋≋" r))
    (println (str "               " wh "\\ | /" r))
    (println (str "                " wh "\\|/" r))
    (println (str "                 " ye b "✦" r))
    (println)
    (println (str "              " mg b "S  Y  L" r))
    (println (str "        " wh "The Ancient Daughter" r))
    (println)
    (println (str cy "  ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋ ≋" r))
    (println (str ye "  ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦ · * · ✦" r))
    (println)
    (println (str "  Model: " wh model r "   Timeout: " wh (/ cmd-timeout-ms 1000) "s" r
                  (when thinking-budget (str "   Thinking: " wh thinking-budget r))
                  "   Type " mg "/help" r " for commands"))
    (println)))

;; --- 13. Shutdown Hook ---
(when *main-script*
  (.addShutdownHook (Runtime/getRuntime)
    (Thread. (fn []
      (println "\n[Syl] Cleaning up container...")
      (nuke-container)))))

;; --- 14. The Main Agent Loop ---
(defn main-loop []
  (start-container)
  (when-not (:no-banner cli-args)
    (print-banner))

  ;; Gather workspace context once at startup for the system prompt
  (let [workspace-ctx  (gather-workspace-context)
        system-blocks  (build-system-blocks workspace-ctx)
        initial-msgs   (if (:resume cli-args)
                         (or (load-session) [])
                         [])]

    (loop [messages initial-msgs]
      (print "\nYou: ")
      (flush)
      (let [user-input (read-line)]

        ;; Handle EOF (Ctrl+D)
        (when (nil? user-input)
          (println "\n[Syl] Farewell. Life before death.")
          (System/exit 0))

        ;; Handle slash commands
        (if-let [slash-result (handle-slash user-input messages)]
          (if (= (:action slash-result) :exit)
            (do
              (println "\n[Syl] Stepping back into the cognitive realm...")
              (println "[Syl] Farewell. Life before death.")
              (System/exit 0))
            (recur (:messages slash-result)))

          ;; Normal message — send to Claude with streaming
          (let [initial-msgs (conj messages {:role "user" :content user-input})
                final-msgs
                (loop [current-msgs initial-msgs]
                  (let [llm-response (call-claude current-msgs system-blocks)]

                    (cond
                      ;; Tool use — execute all tool calls
                      (= (:type llm-response) "tool_use")
                      (let [tool-uses    (:tool-uses llm-response)
                            tool-results (mapv (fn [tu]
                                                (let [tool-name (:name tu)
                                                      output
                                                      (case tool-name
                                                        "run_bash"
                                                        (let [cmd (-> tu :input :command)]
                                                          (println (str "\n$ " cmd))
                                                          (let [res (truncate-output (run-in-container cmd))]
                                                            (println res)
                                                            res))

                                                        "memory_write"
                                                        (let [k (-> tu :input :key)
                                                              c (-> tu :input :content)
                                                              res (memory-write k c)]
                                                          (println (str "\n[Memory] " res))
                                                          res)

                                                        "memory_read"
                                                        (let [k (or (-> tu :input :key) "")
                                                              res (memory-read k)]
                                                          (println (str "\n[Memory] "
                                                                        (if (str/blank? k) "All memories" (str "'" k "'"))
                                                                        " (" (count res) " chars)"))
                                                          res)

                                                        (str "Unknown tool: " tool-name))]
                                                  {:type        "tool_result"
                                                   :tool_use_id (:id tu)
                                                   :content     output}))
                                              tool-uses)
                            next-msgs (conj current-msgs
                                            {:role "assistant" :content (:raw-model-content llm-response)}
                                            {:role "user"      :content tool-results})]
                        (recur next-msgs))

                      ;; Text response — already printed by streaming, just record
                      :else
                      (conj current-msgs {:role "assistant" :content (:raw-model-content llm-response)}))))]

            (recur (compact-history final-msgs))))))))

(when *main-script*
  (main-loop))
