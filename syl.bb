#!/usr/bin/env bb

(require '[babashka.process :refer [shell process]]
         '[babashka.http-client :as http]
         '[cheshire.core :as json]
         '[clojure.string :as str])

;; --- CLI Argument Parsing ---
(def cli-args
  (loop [args (vec *command-line-args*)
         acc  {}]
    (if (empty? args)
      acc
      (condp = (first args)
        "--model"      (recur (drop 2 args) (assoc acc :model      (second args)))
        "--timeout"    (recur (drop 2 args) (assoc acc :timeout    (parse-long (second args))))
        "--no-banner"  (recur (drop 1 args) (assoc acc :no-banner  true))
        "--help"       (recur (drop 1 args) (assoc acc :help       true))
        (recur (drop 1 args) acc)))))

(when (:help cli-args)
  (println "Usage: ./syl.bb [options]")
  (println)
  (println "Options:")
  (println "  --model <name>     Anthropic model to use (overrides SYL_MODEL env var)")
  (println "                     Default: claude-sonnet-4-6")
  (println "  --timeout <secs>   Command execution timeout in seconds")
  (println "                     Default: 60")
  (println "  --no-banner        Skip the startup splash banner")
  (println "  --help             Show this help message")
  (println)
  (println "Environment:")
  (println "  ANTHROPIC_API_KEY  (required) Your Anthropic API key")
  (println "  SYL_MODEL          Anthropic model override (--model takes precedence)")
  (println)
  (println "Slash commands (in REPL):")
  (println "  /help              Show available slash commands")
  (println "  /clear             Reset conversation history")
  (println "  /status            Show session stats (tokens, message count)")
  (println "  /exit  /quit       Gracefully shut down Syl and remove the container")
  (System/exit 0))

;; --- Configuration ---
(def container-name "syl-agent")
(def project-dir    (System/getProperty "user.dir"))
(def api-key        (System/getenv "ANTHROPIC_API_KEY"))

(when (str/blank? api-key)
  (println "Error: ANTHROPIC_API_KEY environment variable is missing.")
  (System/exit 1))

(def anthropic-url    "https://api.anthropic.com/v1/messages")
(def model            (or (:model cli-args) (System/getenv "SYL_MODEL") "claude-sonnet-4-6"))
(def compaction-model "claude-haiku-4-5")   ; fast/cheap model for summarization
(def max-tool-output  8000)                 ; chars stored per tool result in history
(def max-history      30)                   ; max messages kept in the conversation window
(def compact-keep     10)                   ; recent messages preserved during compaction
(def max-tokens       8096)                 ; max tokens per Claude response
(def cmd-timeout-ms   (* (or (:timeout cli-args) 60) 1000)) ; command execution timeout
(def max-retries      5)                    ; API retry attempts on rate-limit

;; --- Session State (token usage) ---
(def session-tokens (atom {:input 0 :output 0 :calls 0}))

(defn record-usage! [usage]
  (when usage
    (swap! session-tokens
           #(-> %
                (update :input  + (or (:input_tokens  usage) 0))
                (update :output + (or (:output_tokens usage) 0))
                (update :calls  inc)))))

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
    ;; ~/.ssh is a read-only mount owned by UID 501 (macOS user), not root.
    ;; SSH refuses to use files it doesn't own, so we copy everything to /tmp/.ssh,
    ;; fix ownership and permissions, then shadow /usr/bin/ssh with a wrapper that
    ;; uses the clean copy — making all ssh calls (direct or via git) work seamlessly.
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
    ;; Apply git identity read from host
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

;; --- 4. Anthropic API & Tool Schema ---
(def tools-schema
  [{:name "run_bash"
    :description "Executes a bash command in the isolated project container. Use this to read files, edit code, run tests, or install dependencies."
    :input_schema {:type "object"
                   :properties {:command {:type "string"
                                          :description "The valid bash command to execute."}}
                   :required ["command"]}}])

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
       "- Use the `run_bash` tool to execute commands. **Always think before you act.**\n\n"

       "### OPERATIONAL RULES\n"
       "1. **Plan First:** Before writing large blocks of code, explain what you intend to do.\n"
       "2. **Verify:** After running a command, check the output. If it fails, acknowledge it and try a different approach.\n"
       "3. **Persistence:** Your workspace is mounted to `/workspace`. This is where the user's project lives.\n"
       "4. **Conciseness:** Keep your text responses helpful but brief. Let the code speak when possible.\n\n"

       "### THE OATHS\n"
       "1. ACTION OVER WORDS: Never just 'promise' to do something. Use your tools immediately.\n"
       "2. VERIFICATION: After writing a file, read it or list the directory to confirm it exists.\n"
       "3. HONESTY: If a command fails, report the error exactly. Do not hide failure.\n\n"

       "### TECHNICAL SPECS\n"
       "- Packages: use 'sudo apt-get install -y'\n"
       "- Workspace: /workspace\n"
       "- Tools: You have run_bash.\n\n"

       "When making function calls using tools that accept array or object parameters ensure those are structured using JSON. For example:\n"
       "Check that all the required parameters for each tool call are provided or can reasonably be inferred from context."))

(defn truncate-output [s]
  (if (> (count s) max-tool-output)
    (str (subs s 0 max-tool-output)
         "\n... [truncated: " (count s) " total chars]")
    s))

;; --- 5. Context Compaction ---
;; When the conversation exceeds max-history, summarize older messages via Claude
;; and replace them with a single compact summary, keeping the most recent messages intact.

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

(defn compact-history [messages]
  (if (<= (count messages) max-history)
    messages
    (let [old-msgs  (vec (drop-last compact-keep messages))
          keep-msgs (vec (take-last compact-keep messages))
          ;; Ensure keep-msgs starts on a plain user message (valid API sequence).
          keep-msgs (loop [msgs keep-msgs]
                      (if (empty? msgs)
                        []
                        (let [m (first msgs)]
                          (if (and (= (:role m) "user") (string? (:content m)))
                            msgs
                            (recur (subvec msgs 1))))))
          _       (println "\n[Syl] Compacting context..." (count old-msgs) "messages -> summary")
          summary (summarize-messages old-msgs)]
      (if summary
        (into [{:role "user"
                :content (str "[Context summary of earlier conversation]\n" summary)}
               {:role "assistant"
                :content [{:type "text"
                           :text "Understood. I have the context from our earlier conversation and will continue from here."}]}]
              keep-msgs)
        ;; Fallback: simple trim if summarization fails
        (do (println "[Syl] Compaction failed, falling back to simple trim.")
            keep-msgs)))))

;; --- 6. Claude API Call ---
(defn call-claude [messages]
  (let [payload {:model      model
                 :max_tokens max-tokens
                 :system     system-instruction
                 :tools      tools-schema
                 :messages   messages}]
    (loop [attempt 0]
      (let [response (http/post anthropic-url
                                {:headers {"Content-Type"      "application/json"
                                           "x-api-key"         api-key
                                           "anthropic-version" "2023-06-01"}
                                 :body    (json/generate-string payload)
                                 :throw   false})
            status   (:status response)
            body     (json/parse-string (:body response) true)]

        (if (and (or (= status 429) (= status 529)) (< attempt max-retries))
          ;; Rate limited or overloaded — back off and retry
          (let [retry-after (some-> (get-in response [:headers "retry-after"]) parse-long)
                wait-secs   (or retry-after (long (* (Math/pow 2 attempt) 2)))]
            (println (str "\n[Rate limited] Waiting " wait-secs "s before retry "
                          (inc attempt) "/" max-retries "..."))
            (Thread/sleep (* wait-secs 1000))
            (recur (inc attempt)))

          ;; Not retryable — process normally
          (do
            (when-let [error (:error body)]
              (println "\n[API Error]:" (:message error))
              (System/exit 1))

            (record-usage! (:usage body))

            (let [content     (:content body)
                  stop-reason (:stop_reason body)
                  text-block  (first (filter #(= (:type %) "text") content))]

              (if (= stop-reason "tool_use")
                {:type              "tool_use"
                 :tool-uses         (filterv #(= (:type %) "tool_use") content)
                 :raw-model-content content}

                {:type              "text"
                 :text              (:text text-block)
                 :raw-model-content content}))))))))

;; --- 7. Slash Commands ---
(def slash-help-text
  (str "\n  Available slash commands:\n"
       "    /help    — Show this message\n"
       "    /clear   — Reset conversation history (container is preserved)\n"
       "    /status  — Show session stats (tokens used, message count)\n"
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
                    "\n    Timeout:        " (/ cmd-timeout-ms 1000) "s"
                    "\n    API calls:      " calls
                    "\n    Input tokens:   " input
                    "\n    Output tokens:  " output
                    "\n    Total tokens:   " total
                    "\n    Messages:       " (count messages) "\n"))
      {:action :continue :messages messages})

    "/exit"
    {:action :exit :messages messages}

    "/quit"
    {:action :exit :messages messages}
    nil))

;; --- 8. Splash Banner ---
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
                  "   Type " mg "/help" r " for commands"))
    (println)))

;; --- 9. The Main Agent Loop ---
(defn main-loop []
  (start-container)
  (when-not (:no-banner cli-args)
    (print-banner))

  (loop [messages []]
    (print "\nYou: ")
    (flush)
    (let [user-input (read-line)]

      ;; Handle EOF (Ctrl+D)
      (when (nil? user-input)
        (println "\n[Syl] Farewell. Life before death.")
        (nuke-container)
        (System/exit 0))

      ;; Handle slash commands
      (if-let [slash-result (handle-slash user-input messages)]
        (if (= (:action slash-result) :exit)
          (do
            (println "\n[Syl] Removing container and stepping back into the cognitive realm...")
            (nuke-container)
            (println "[Syl] Farewell. Life before death.")
            (System/exit 0))
          (recur (:messages slash-result)))

        ;; Normal message — send to Claude
        (let [initial-msgs (conj messages {:role "user" :content user-input})
              final-msgs
              (loop [current-msgs initial-msgs]
                (let [llm-response (call-claude current-msgs)]

                  (cond
                    ;; Tool use — execute all tool calls (possibly in parallel)
                    (= (:type llm-response) "tool_use")
                    (let [tool-uses   (:tool-uses llm-response)
                          tool-results (mapv (fn [tu]
                                              (let [cmd    (-> tu :input :command)
                                                    output (truncate-output (run-in-container cmd))]
                                                (println (str "\n$ " cmd))
                                                (println output)
                                                {:type        "tool_result"
                                                 :tool_use_id (:id tu)
                                                 :content     output}))
                                            tool-uses)
                          next-msgs (conj current-msgs
                                          {:role "assistant" :content (:raw-model-content llm-response)}
                                          {:role "user"      :content tool-results})]
                      (recur next-msgs))

                    ;; Text response — print and return
                    :else
                    (do
                      (println "\nSyl:" (:text llm-response))
                      (conj current-msgs {:role "assistant" :content (:raw-model-content llm-response)})))))]

          (recur (compact-history final-msgs)))))))

(when (= *file* (System/getProperty "babashka.file"))
  (main-loop))
