#!/usr/bin/env bb

(require '[babashka.process :refer [shell]]
         '[babashka.http-client :as http]
         '[cheshire.core :as json]
         '[clojure.string :as str])

;; --- Configuration ---
(def container-name "syl-agent")
(def project-dir (System/getProperty "user.dir"))
(def api-key (System/getenv "GEMINI_API_KEY"))

(when (str/blank? api-key)
  (println "Error: GEMINI_API_KEY environment variable is missing.")
  (System/exit 1))

(def gemini-url 
  (str "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent?key=" api-key))

;; --- 1. Container Management ---
(defn nuke-container []
  (shell {:continue true :out :string :err :string} 
         (str "podman rm -f " container-name)))

(defn start-container []
  (nuke-container)
  (println "Summoning isolated container in:" project-dir)
  (shell (str "podman run -d --name " container-name " "
              "-v " project-dir ":/workspace:rw "
              "--workdir /workspace "
              "-it alpine:latest /bin/sh")))

;; --- 2. The Execution Bridge ---
(defn run-in-container [cmd]
  (let [wrapped-cmd ["systemd-run" "--scope" "--user" "timeout" "60s" 
                     "podman" "exec" container-name "/bin/sh" "-c" cmd]
        result (apply shell {:out :string :err :string :continue true} wrapped-cmd)
        exit-code (:exit result)
        output (if (str/blank? (:out result)) (:err result) (:out result))]
    (if (= 0 exit-code)
      output
      (str "Error (Exit " exit-code "): " output))))

;; --- 3. Gemini API & Tool Schema ---
(def tools-schema
  [{:functionDeclarations
    [{:name "run_bash"
      :description "Executes a bash command in the isolated project container. Use this to read files, edit code, run tests, or install dependencies."
      :parameters {:type "OBJECT"
                   :properties {:command {:type "STRING"
                                          :description "The valid bash command to execute."}}
                   :required ["command"]}}]}])

(defn call-gemini [messages]
  (let [payload {:systemInstruction {:parts [{:text "You are Syl, a highly capable coding agent. You live in a containerized environment. Use the run_bash tool to navigate, write code, and solve the user's problems. If a command fails, read the error and try a different approach."}]}
                 :contents messages
                 :tools tools-schema}
        
        response (http/post gemini-url
                            {:headers {"Content-Type" "application/json"}
                             :body (json/generate-string payload)
                             :throw false})
        body (json/parse-string (:body response) true)]
    
    (when-let [error (:error body)]
      (println "\n[API Error]:" (:message error))
      (System/exit 1))
      
    (let [candidate (-> body :candidates first)
          content (-> candidate :content)
          part (-> content :parts first)]
      
      (if-let [func-call (:functionCall part)]
        {:type "tool_use"
         :name (:name func-call)
         :command (-> func-call :args :command)
         :raw-model-msg content} 
        
        {:type "text"
         :text (:text part)
         :raw-model-msg content}))))

;; --- 4. The Main Agent Loop ---
(defn main-loop []
  (start-container)
  (println "\n--- Syl Harness Ready (Powered by Gemini) ---")
  
  (loop [messages []]
    (print "\nYou: ")
    (flush)
    (let [user-input (read-line)
          initial-msgs (conj messages {:role "user" :parts [{:text user-input}]})]
      
      (let [final-msgs 
            (loop [current-msgs initial-msgs]
              (let [llm-response (call-gemini current-msgs)]
                
                (cond
                  (= (:type llm-response) "tool_use")
                  (let [cmd (:command llm-response)
                        model-msg (:raw-model-msg llm-response)
                        _ (println "\n[Syl Executing] ->" cmd)
                        
                        tool-result (run-in-container cmd)
                        _ (println "[Result]\n" (subs tool-result 0 (min 500 (count tool-result))) 
                                   (if (> (count tool-result) 500) "...\n(Output truncated)" ""))
                        
                        next-msgs (-> current-msgs
                                      (conj model-msg)
                                      (conj {:role "user" 
                                             :parts [{:functionResponse
                                                      {:name (:name llm-response)
                                                       :response {:output tool-result}}}]}))]
                    (recur next-msgs))
                  
                  :else
                  (do
                    (println "\nSyl:" (:text llm-response))
                    (conj current-msgs (:raw-model-msg llm-response))))))]
        
        (recur final-msgs)))))

(when (= *file* (System/getProperty "babashka.file"))
  (main-loop))
