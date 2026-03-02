#!/usr/bin/env bb

(require '[babashka.process :refer [shell process]]
         '[babashka.http-client :as http]
         '[cheshire.core :as json]
         '[clojure.string :as str])

;; --- Configuration ---
(def container-name "syl-agent")
(def project-dir (System/getProperty "user.dir"))
(def api-key (System/getenv "ANTHROPIC_API_KEY"))

(when (str/blank? api-key)
  (println "Error: ANTHROPIC_API_KEY environment variable is missing.")
  (System/exit 1))

(def anthropic-url "https://api.anthropic.com/v1/messages")
(def model "claude-opus-4-6")
(def max-tool-output 8000) ; chars of tool output stored per result in history
(def max-history 30)       ; max messages kept in the conversation window

;; --- 1. Container Management ---
(defn nuke-container []
  (shell {:continue true :out :string :err :string}
         (str "podman rm -f " container-name)))

(defn configure-git-and-ssh []
  (let [home      (System/getProperty "user.home")
        git-name  (str/trim (:out (shell {:out :string :err :string :continue true}
                                         "git config --global user.name")))
        git-email (str/trim (:out (shell {:out :string :err :string :continue true}
                                         "git config --global user.email")))
        ssh-src   (str home "/.ssh")]
    ;; Copy SSH keys and fix permissions
    (when (.exists (java.io.File. ssh-src))
      (println "[Syl Setup] Copying SSH keys into container...")
      (shell {:continue true :out :string :err :string}
             (str "podman cp " ssh-src " " container-name ":/root/"))
      (shell {:continue true :out :string :err :string}
             (str "podman exec " container-name
                  " bash -c 'chmod 700 /root/.ssh"
                  " && find /root/.ssh -type f -exec chmod 600 {} \\;"
                  " && find /root/.ssh -name \"*.pub\" -exec chmod 644 {} \\;"
                  " && ssh-keyscan -H github.com >> /root/.ssh/known_hosts 2>/dev/null"
                  " && true'")))
    ;; Apply git identity read from host
    (when-not (str/blank? git-name)
      (println "[Syl Setup] Configuring git identity as:" git-name git-email)
      (shell {:continue true :out :string :err :string}
             (str "podman exec " container-name
                  " git config --global user.name '" git-name "'"))
      (shell {:continue true :out :string :err :string}
             (str "podman exec " container-name
                  " git config --global user.email '" git-email "'")))))

(defn start-container []
  (nuke-container)
  (println "Summoning a more powerful Syl in:" project-dir)
  (shell (str "podman run -d --name " container-name " "
              "--memory=2g "
              "-v " project-dir ":/workspace:rw "
              "--workdir /workspace "
              "-it mcr.microsoft.com/devcontainers/base:ubuntu /bin/bash"))
  (configure-git-and-ssh))

;; --- 2. The Execution Bridge ---
(defn run-in-container [cmd]
  (let [p (process ["podman" "exec" container-name "/bin/bash" "-c" cmd]
                   {:out :string :err :string})
        result (deref p 60000 :timeout)]

    (if (= result :timeout)
      (do
        (.destroy p)
        "Error: Command timed out after 60 seconds.")
      (let [{:keys [exit out err]} result]
        (if (= 0 exit)
          out
          (str "Error (Exit " exit "): " (if (str/blank? out) err out)))))))

;; --- 3. Anthropic API & Tool Schema ---
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
       "- Tools: You have run_bash."))

(defn truncate-output [s]
  (if (> (count s) max-tool-output)
    (str (subs s 0 max-tool-output)
         "\n... [truncated: " (count s) " total chars]")
    s))

;; Trim the message history to a sliding window, always starting on a plain user message.
(defn trim-history [messages]
  (if (<= (count messages) max-history)
    messages
    (loop [msgs (vec (take-last max-history messages))]
      (if (empty? msgs)
        messages
        (let [m (first msgs)]
          (if (and (= (:role m) "user") (string? (:content m)))
            msgs
            (recur (subvec msgs 1))))))))

(defn call-claude [messages]
  (let [payload {:model model
                 :max_tokens 8096
                 :system system-instruction
                 :tools tools-schema
                 :messages messages}

        response (http/post anthropic-url
                            {:headers {"Content-Type"      "application/json"
                                       "x-api-key"         api-key
                                       "anthropic-version" "2023-06-01"}
                             :body (json/generate-string payload)
                             :throw false})
        body (json/parse-string (:body response) true)]

    (when-let [error (:error body)]
      (println "\n[API Error]:" (:message error))
      (System/exit 1))

    (let [content     (:content body)
          stop-reason (:stop_reason body)
          text-block  (first (filter #(= (:type %) "text") content))]

      (if (= stop-reason "tool_use")
        {:type              "tool_use"
         :tool-uses         (filterv #(= (:type %) "tool_use") content)
         :raw-model-content content}

        {:type              "text"
         :text              (:text text-block)
         :raw-model-content content}))))

;; --- 4. The Main Agent Loop ---
(defn main-loop []
  (start-container)
  (println "\n--- Syl Harness Ready (Powered by Claude) ---")

  (loop [messages []]
    (print "\nYou: ")
    (flush)
    (let [user-input   (read-line)
          initial-msgs (conj messages {:role "user" :content user-input})
          final-msgs
          (loop [current-msgs initial-msgs]
              (let [llm-response (call-claude current-msgs)]

                (cond
                  (= (:type llm-response) "tool_use")
                  (let [model-content (:raw-model-content llm-response)
                        tool-results  (mapv (fn [tu]
                                              (let [cmd (-> tu :input :command)
                                                    _   (println "\n[Syl Executing] ->" cmd)
                                                    res (run-in-container cmd)
                                                    _   (println "[Result]\n"
                                                                 (subs res 0 (min 500 (count res)))
                                                                 (if (> (count res) 500) "...\n(Output truncated)" ""))]
                                                {:type        "tool_result"
                                                 :tool_use_id (:id tu)
                                                 :content     (truncate-output res)}))
                                            (:tool-uses llm-response))
                        next-msgs (-> current-msgs
                                      (conj {:role "assistant" :content model-content})
                                      (conj {:role "user" :content tool-results}))]
                    (recur next-msgs))

                  :else
                  (do
                    (println "\nSyl:" (:text llm-response))
                    (conj current-msgs {:role "assistant" :content (:raw-model-content llm-response)})))))]

        (recur (trim-history final-msgs)))))

(when (= *file* (System/getProperty "babashka.file"))
  (main-loop))
