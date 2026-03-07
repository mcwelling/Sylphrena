#!/usr/bin/env bb

;; Test suite for Sylphrena
;; Run: bb test_syl.bb  or  bb test

(require '[clojure.test :refer [deftest is testing run-tests]]
         '[clojure.string :as str]
         '[cheshire.core :as json])

(import '[java.io ByteArrayInputStream])

;; Load syl.bb as a library (guards prevent System/exit and shutdown hooks)
(load-file "syl.bb")

;; ============================================================
;; 1. CLI Argument Parsing
;; ============================================================

(deftest test-parse-cli-args-empty
  (is (= {} (parse-cli-args []))))

(deftest test-parse-cli-args-model
  (is (= {:model "claude-opus-4-6"}
         (parse-cli-args ["--model" "claude-opus-4-6"]))))

(deftest test-parse-cli-args-timeout
  (is (= {:timeout 120}
         (parse-cli-args ["--timeout" "120"]))))

(deftest test-parse-cli-args-thinking
  (is (= {:thinking 4000}
         (parse-cli-args ["--thinking" "4000"]))))

(deftest test-parse-cli-args-boolean-flags
  (is (= {:resume true :no-banner true}
         (parse-cli-args ["--resume" "--no-banner"]))))

(deftest test-parse-cli-args-help
  (is (= {:help true}
         (parse-cli-args ["--help"]))))

(deftest test-parse-cli-args-combined
  (is (= {:model "claude-haiku-4-5" :timeout 30 :resume true}
         (parse-cli-args ["--model" "claude-haiku-4-5" "--timeout" "30" "--resume"]))))

(deftest test-parse-cli-args-unknown-flags-ignored
  (is (= {:model "test"}
         (parse-cli-args ["--unknown" "--model" "test" "--bogus"]))))

;; ============================================================
;; 2. truncate-output
;; ============================================================

(deftest test-truncate-output-short-string
  (is (= "hello" (truncate-output "hello"))))

(deftest test-truncate-output-at-limit
  (let [s (apply str (repeat max-tool-output "x"))]
    (is (= s (truncate-output s)))))

(deftest test-truncate-output-over-limit
  (let [s (apply str (repeat (+ max-tool-output 100) "x"))
        result (truncate-output s)]
    (testing "truncated output starts with first max-tool-output chars"
      (is (str/starts-with? result (subs s 0 max-tool-output))))
    (testing "truncated output includes total char count"
      (is (str/includes? result (str (count s) " total chars"))))
    (testing "truncated output is shorter than original"
      (is (< (count result) (count s))))))

(deftest test-truncate-output-empty
  (is (= "" (truncate-output ""))))

;; ============================================================
;; 3. find-valid-start
;; ============================================================

(deftest test-find-valid-start-already-valid
  (let [msgs [{:role "user" :content "hello"}
              {:role "assistant" :content [{:type "text" :text "hi"}]}]]
    (is (= msgs (find-valid-start msgs)))))

(deftest test-find-valid-start-skips-tool-result
  (let [tool-msg {:role "user" :content [{:type "tool_result" :tool_use_id "t1" :content "ok"}]}
        user-msg {:role "user" :content "hello"}
        asst-msg {:role "assistant" :content [{:type "text" :text "hi"}]}]
    (is (= [user-msg asst-msg]
           (find-valid-start [tool-msg user-msg asst-msg])))))

(deftest test-find-valid-start-skips-assistant-then-tool-result
  (let [asst {:role "assistant" :content [{:type "tool_use" :id "t1" :name "run_bash" :input {:command "ls"}}]}
        tool {:role "user" :content [{:type "tool_result" :tool_use_id "t1" :content "ok"}]}
        user {:role "user" :content "now what?"}]
    (is (= [user] (find-valid-start [asst tool user])))))

(deftest test-find-valid-start-all-tool-exchanges
  (let [asst {:role "assistant" :content [{:type "tool_use" :id "t1" :name "run_bash" :input {:command "ls"}}]}
        tool {:role "user" :content [{:type "tool_result" :tool_use_id "t1" :content "ok"}]}]
    (is (= [] (find-valid-start [asst tool asst tool])))))

(deftest test-find-valid-start-empty
  (is (= [] (find-valid-start []))))

;; ============================================================
;; 4. build-compacted-messages
;; ============================================================

(deftest test-build-compacted-messages-with-summary
  (let [keep [{:role "user" :content "what next?"}]
        result (build-compacted-messages "User asked about X. Syl did Y." keep)]
    (testing "result starts with summary user message"
      (is (= "user" (:role (first result))))
      (is (str/includes? (:content (first result)) "Context summary")))
    (testing "result has assistant ack"
      (is (= "assistant" (:role (second result)))))
    (testing "result ends with kept messages"
      (is (= "what next?" (:content (last result)))))
    (testing "result has 3 messages total"
      (is (= 3 (count result))))))

(deftest test-build-compacted-messages-nil-summary
  (let [keep [{:role "user" :content "still here"}]
        result (build-compacted-messages nil keep)]
    (testing "fallback returns just the kept messages"
      (is (= keep result)))))

;; ============================================================
;; 5. Memory (file I/O with temp directory)
;; ============================================================

(deftest test-memory-write-and-read
  (let [tmp (str (System/getProperty "java.io.tmpdir") "/syl-test-memory-" (System/currentTimeMillis))
        _ (alter-var-root #'memory-dir (constantly tmp))]
    (try
      (testing "write returns confirmation"
        (let [result (memory-write "test-topic" "Some notes here")]
          (is (str/includes? result "Remembered"))
          (is (str/includes? result "test-topic"))))

      (testing "read back specific key"
        (is (= "Some notes here" (memory-read "test-topic"))))

      (testing "read nonexistent key"
        (is (str/includes? (memory-read "nope") "No memory found")))

      (testing "read all memories"
        (memory-write "another" "More notes")
        (let [all (memory-read "")]
          (is (str/includes? all "test-topic"))
          (is (str/includes? all "another"))
          (is (str/includes? all "Some notes here"))
          (is (str/includes? all "More notes"))))

      (testing "load-all-memories returns combined content"
        (let [all (load-all-memories)]
          (is (string? all))
          (is (str/includes? all "test-topic"))
          (is (str/includes? all "another"))))

      (finally
        ;; Cleanup
        (doseq [f (.listFiles (java.io.File. tmp))]
          (.delete f))
        (.delete (java.io.File. tmp))
        (alter-var-root #'memory-dir (constantly (str project-dir "/.syl/memory")))))))

(deftest test-memory-read-empty-dir
  (let [tmp (str (System/getProperty "java.io.tmpdir") "/syl-test-empty-" (System/currentTimeMillis))
        _ (.mkdirs (java.io.File. tmp))
        _ (alter-var-root #'memory-dir (constantly tmp))]
    (try
      (is (= "No memories stored yet." (memory-read "")))
      (is (nil? (load-all-memories)))
      (finally
        (.delete (java.io.File. tmp))
        (alter-var-root #'memory-dir (constantly (str project-dir "/.syl/memory")))))))

;; ============================================================
;; 6. Session Persistence (temp directory)
;; ============================================================

(deftest test-session-save-and-load
  (let [tmp-dir  (str (System/getProperty "java.io.tmpdir") "/syl-test-session-" (System/currentTimeMillis))
        tmp-file (str tmp-dir "/session.json")
        msgs     [{:role "user" :content "hello"}
                  {:role "assistant" :content [{:type "text" :text "hi there"}]}]]
    (alter-var-root #'session-dir (constantly tmp-dir))
    (alter-var-root #'session-file (constantly tmp-file))
    (try
      (testing "save-session! creates file"
        (save-session! msgs)
        (is (.exists (java.io.File. tmp-file))))

      (testing "load-session round-trips messages"
        (let [loaded (load-session)]
          (is (= (count msgs) (count loaded)))
          (is (= "user" (:role (first loaded))))
          (is (= "hello" (:content (first loaded))))))

      (finally
        (.delete (java.io.File. tmp-file))
        (.delete (java.io.File. tmp-dir))
        (alter-var-root #'session-dir (constantly (str project-dir "/.syl")))
        (alter-var-root #'session-file (constantly (str project-dir "/.syl/session.json")))))))

(deftest test-load-session-missing-file
  (let [tmp-dir  (str (System/getProperty "java.io.tmpdir") "/syl-test-no-session-" (System/currentTimeMillis))
        tmp-file (str tmp-dir "/session.json")]
    (alter-var-root #'session-dir (constantly tmp-dir))
    (alter-var-root #'session-file (constantly tmp-file))
    (try
      (is (nil? (load-session)))
      (finally
        (alter-var-root #'session-dir (constantly (str project-dir "/.syl")))
        (alter-var-root #'session-file (constantly (str project-dir "/.syl/session.json")))))))

;; ============================================================
;; 7. SSE Stream Parsing
;; ============================================================

(defn make-sse-stream [lines]
  (ByteArrayInputStream. (.getBytes (str/join "\n" lines) "UTF-8")))

(deftest test-parse-sse-text-event
  (let [events (atom [])
        stream (make-sse-stream
                 ["event: content_block_delta"
                  (str "data: " (json/generate-string {:type "content_block_delta"
                                                       :index 0
                                                       :delta {:type "text_delta" :text "Hello"}}))
                  ""])]
    (parse-sse-stream stream (fn [evt data] (swap! events conj {:event evt :data data})))
    (is (= 1 (count @events)))
    (is (= "content_block_delta" (:event (first @events))))
    (is (= "text_delta" (-> @events first :data :delta :type)))))

(deftest test-parse-sse-tool-use-event
  (let [events (atom [])
        stream (make-sse-stream
                 ["event: content_block_start"
                  (str "data: " (json/generate-string {:type "content_block_start"
                                                       :index 0
                                                       :content_block {:type "tool_use"
                                                                       :id "toolu_123"
                                                                       :name "run_bash"
                                                                       :input {}}}))
                  ""])]
    (parse-sse-stream stream (fn [evt data] (swap! events conj {:event evt :data data})))
    (is (= 1 (count @events)))
    (is (= "tool_use" (-> @events first :data :content_block :type)))))

(deftest test-parse-sse-thinking-event
  (let [events (atom [])
        stream (make-sse-stream
                 ["event: content_block_delta"
                  (str "data: " (json/generate-string {:type "content_block_delta"
                                                       :index 0
                                                       :delta {:type "thinking_delta" :thinking "Let me think..."}}))
                  ""])]
    (parse-sse-stream stream (fn [evt data] (swap! events conj data)))
    (is (= "thinking_delta" (-> @events first :delta :type)))
    (is (= "Let me think..." (-> @events first :delta :thinking)))))

(deftest test-parse-sse-done-ignored
  (let [events (atom [])
        stream (make-sse-stream
                 ["event: message_stop"
                  (str "data: " (json/generate-string {:type "message_stop"}))
                  ""
                  "data: [DONE]"
                  ""])]
    (parse-sse-stream stream (fn [_evt data] (swap! events conj data)))
    (is (= 1 (count @events)))
    (is (= "message_stop" (:type (first @events))))))

(deftest test-parse-sse-multiple-events
  (let [events (atom [])
        stream (make-sse-stream
                 ["event: message_start"
                  (str "data: " (json/generate-string {:type "message_start" :message {:usage {:input_tokens 100}}}))
                  ""
                  "event: content_block_start"
                  (str "data: " (json/generate-string {:type "content_block_start" :index 0 :content_block {:type "text" :text ""}}))
                  ""
                  "event: content_block_delta"
                  (str "data: " (json/generate-string {:type "content_block_delta" :index 0 :delta {:type "text_delta" :text "Hi"}}))
                  ""
                  "data: [DONE]"
                  ""])]
    (parse-sse-stream stream (fn [_evt data] (swap! events conj data)))
    (is (= 3 (count @events)))
    (is (= "message_start" (:type (first @events))))
    (is (= "content_block_start" (:type (second @events))))
    (is (= "content_block_delta" (:type (nth @events 2))))))

(deftest test-parse-sse-blank-lines-between-events
  (let [events (atom [])
        stream (make-sse-stream
                 [""
                  ""
                  "event: message_start"
                  (str "data: " (json/generate-string {:type "message_start" :message {}}))
                  ""
                  ""
                  ""])]
    (parse-sse-stream stream (fn [_evt data] (swap! events conj data)))
    (is (= 1 (count @events)))))

;; ============================================================
;; 8. Workflow Parsing
;; ============================================================

(deftest test-parse-workflow-basic
  (let [content "# Ship\nTest and push.\n\n- Run tests: `bb test`\n- Push: `git push`\n"
        wf (parse-workflow content)]
    (is (= "Ship" (:name wf)))
    (is (= "Test and push." (:description wf)))
    (is (= 2 (count (:steps wf))))
    (is (= "Run tests" (:label (first (:steps wf)))))
    (is (= "bb test" (:command (first (:steps wf)))))
    (is (= "Push" (:label (second (:steps wf)))))
    (is (= "git push" (:command (second (:steps wf)))))))

(deftest test-parse-workflow-no-heading
  (let [content "- Step one: `echo hello`\n- Step two: `echo world`\n"
        wf (parse-workflow content)]
    (is (= "unnamed" (:name wf)))
    (is (= 2 (count (:steps wf))))))

(deftest test-parse-workflow-no-steps
  (let [content "# Empty\nJust a heading, no steps.\n"]
    (is (nil? (parse-workflow content)))))

(deftest test-parse-workflow-asterisk-bullets
  (let [content "# Deploy\n* Build: `npm run build`\n* Deploy: `npm run deploy`\n"
        wf (parse-workflow content)]
    (is (= 2 (count (:steps wf))))
    (is (= "npm run build" (:command (first (:steps wf)))))))

(deftest test-list-workflows-empty
  (let [tmp (str (System/getProperty "java.io.tmpdir") "/syl-test-wf-empty-" (System/currentTimeMillis))
        _ (.mkdirs (java.io.File. tmp))
        _ (alter-var-root #'workflow-dir (constantly tmp))]
    (try
      (is (empty? (list-workflows)))
      (finally
        (.delete (java.io.File. tmp))
        (alter-var-root #'workflow-dir (constantly (str project-dir "/.syl/workflows")))))))

(deftest test-list-and-load-workflows
  (let [tmp (str (System/getProperty "java.io.tmpdir") "/syl-test-wf-" (System/currentTimeMillis))
        _ (.mkdirs (java.io.File. tmp))
        _ (alter-var-root #'workflow-dir (constantly tmp))]
    (try
      (spit (str tmp "/ship.md") "# Ship\nPush code.\n\n- Test: `bb test`\n- Push: `git push`\n")
      (spit (str tmp "/check.md") "# Check\nRun checks.\n\n- Lint: `bb lint`\n")

      (testing "list-workflows returns sorted entries"
        (let [wfs (list-workflows)]
          (is (= 2 (count wfs)))
          (is (= "check" (:key (first wfs))))
          (is (= "ship" (:key (second wfs))))))

      (testing "load-workflow parses correctly"
        (let [wf (load-workflow "ship")]
          (is (= "Ship" (:name wf)))
          (is (= 2 (count (:steps wf))))))

      (testing "load-workflow returns nil for missing"
        (is (nil? (load-workflow "nope"))))

      (finally
        (doseq [f (.listFiles (java.io.File. tmp))]
          (.delete f))
        (.delete (java.io.File. tmp))
        (alter-var-root #'workflow-dir (constantly (str project-dir "/.syl/workflows")))))))

(deftest test-workflow-write-and-delete
  (let [tmp (str (System/getProperty "java.io.tmpdir") "/syl-test-wf-write-" (System/currentTimeMillis))
        _ (alter-var-root #'workflow-dir (constantly tmp))]
    (try
      (let [content "# Deploy\nDeploy to prod.\n\n- Build: `npm run build`\n- Deploy: `npm run deploy`\n"]

        (testing "workflow-write creates a new workflow"
          (let [res (workflow-write "deploy" content)]
            (is (str/includes? res "created"))
            (is (str/includes? res "2 steps"))
            (is (.exists (java.io.File. (str tmp "/deploy.md"))))))

        (testing "workflow-write rejects duplicate name"
          (let [res (workflow-write "deploy" content)]
            (is (str/starts-with? res "Error"))
            (is (str/includes? res "already exists"))))

        (testing "workflow-write rejects invalid format"
          (let [res (workflow-write "bad" "# Bad\nNo steps here.\n")]
            (is (str/starts-with? res "Error"))
            (is (str/includes? res "Invalid workflow format"))))

        (testing "workflow-delete removes an existing workflow"
          (let [res (workflow-delete "deploy")]
            (is (str/includes? res "deleted"))
            (is (not (.exists (java.io.File. (str tmp "/deploy.md")))))))

        (testing "workflow-delete returns error for missing"
          (let [res (workflow-delete "deploy")]
            (is (str/starts-with? res "Error"))
            (is (str/includes? res "not found"))))

        (testing "workflow-write succeeds after delete (no collision)"
          (let [res (workflow-write "deploy" content)]
            (is (str/includes? res "created")))))

      (finally
        (doseq [f (.listFiles (java.io.File. tmp))]
          (.delete f))
        (.delete (java.io.File. tmp))
        (alter-var-root #'workflow-dir (constantly (str project-dir "/.syl/workflows")))))))

;; ============================================================
;; 9. Slash Commands (including /workflow)
;; ============================================================

(deftest test-slash-help
  (let [msgs [{:role "user" :content "hi"}]
        result (handle-slash "/help" msgs)]
    (is (= :continue (:action result)))
    (is (= msgs (:messages result)))))

(deftest test-slash-clear
  (let [msgs [{:role "user" :content "hi"}]
        result (handle-slash "/clear" msgs)]
    (is (= :continue (:action result)))
    (is (= [] (:messages result)))))

(deftest test-slash-status
  (let [msgs [{:role "user" :content "hi"}]
        result (handle-slash "/status" msgs)]
    (is (= :continue (:action result)))
    (is (= msgs (:messages result)))))

(deftest test-slash-save
  (let [tmp-dir  (str (System/getProperty "java.io.tmpdir") "/syl-test-slash-save-" (System/currentTimeMillis))
        tmp-file (str tmp-dir "/session.json")]
    (alter-var-root #'session-dir (constantly tmp-dir))
    (alter-var-root #'session-file (constantly tmp-file))
    (try
      (let [msgs [{:role "user" :content "test"}]
            result (handle-slash "/save" msgs)]
        (is (= :continue (:action result)))
        (is (= msgs (:messages result)))
        (is (.exists (java.io.File. tmp-file))))
      (finally
        (.delete (java.io.File. tmp-file))
        (.delete (java.io.File. tmp-dir))
        (alter-var-root #'session-dir (constantly (str project-dir "/.syl")))
        (alter-var-root #'session-file (constantly (str project-dir "/.syl/session.json")))))))

(deftest test-slash-exit
  (let [result (handle-slash "/exit" [])]
    (is (= :exit (:action result)))))

(deftest test-slash-quit
  (let [result (handle-slash "/quit" [])]
    (is (= :exit (:action result)))))

(deftest test-slash-unknown-returns-nil
  (is (nil? (handle-slash "hello world" [])))
  (is (nil? (handle-slash "/unknown" []))))

(deftest test-slash-with-whitespace
  (let [result (handle-slash "  /help  " [])]
    (is (= :continue (:action result)))))

(deftest test-slash-workflow-list
  (let [result (handle-slash "/workflow" [{:role "user" :content "hi"}])]
    (is (= :continue (:action result)))))

(deftest test-slash-workflow-missing
  (let [tmp (str (System/getProperty "java.io.tmpdir") "/syl-test-wf-slash-" (System/currentTimeMillis))
        _ (.mkdirs (java.io.File. tmp))
        _ (alter-var-root #'workflow-dir (constantly tmp))]
    (try
      (let [result (handle-slash "/workflow nonexistent" [])]
        (is (= :continue (:action result))))
      (finally
        (.delete (java.io.File. tmp))
        (alter-var-root #'workflow-dir (constantly (str project-dir "/.syl/workflows")))))))

;; ============================================================
;; 10. Rate Limit Budgeting
;; ============================================================

(deftest test-record-usage
  (let [original @session-tokens]
    (try
      (reset! session-tokens {:input 0 :output 0 :calls 0})
      (record-usage! {:input_tokens 100 :output_tokens 50})
      (is (= 100 (:input @session-tokens)))
      (is (= 50 (:output @session-tokens)))
      (is (= 1 (:calls @session-tokens)))

      (record-usage! {:input_tokens 200 :output_tokens 30})
      (is (= 300 (:input @session-tokens)))
      (is (= 80 (:output @session-tokens)))
      (is (= 2 (:calls @session-tokens)))

      (testing "nil usage is a no-op"
        (record-usage! nil)
        (is (= 2 (:calls @session-tokens))))

      (finally
        (reset! session-tokens original)))))

(deftest test-rate-call-tracking
  (let [original @recent-api-calls]
    (try
      (reset! recent-api-calls [])
      (record-rate-call! 1000)
      (is (= 1 (count @recent-api-calls)))
      (is (= 1000 (:tokens (first @recent-api-calls))))

      (record-rate-call! 2000)
      (is (= 2 (count @recent-api-calls)))

      (testing "prune removes old entries"
        (reset! recent-api-calls [{:ts 0 :tokens 500}
                                  {:ts (System/currentTimeMillis) :tokens 1000}])
        (prune-old-calls!)
        (is (= 1 (count @recent-api-calls)))
        (is (= 1000 (:tokens (first @recent-api-calls)))))

      (finally
        (reset! recent-api-calls original)))))

;; ============================================================
;; 11. System Prompt Construction
;; ============================================================

(deftest test-build-system-blocks-structure
  (let [tmp (str (System/getProperty "java.io.tmpdir") "/syl-test-sysprompt-" (System/currentTimeMillis))
        _ (alter-var-root #'memory-dir (constantly tmp))]
    (try
      (let [blocks (build-system-blocks "### WORKSPACE\nFiles: syl.bb")]
        (is (= 1 (count blocks)))
        (is (= "text" (:type (first blocks))))
        (is (str/includes? (:text (first blocks)) "Syl"))
        (is (str/includes? (:text (first blocks)) "WORKSPACE"))
        (is (= {:type "ephemeral"} (:cache_control (first blocks)))))
      (finally
        (alter-var-root #'memory-dir (constantly (str project-dir "/.syl/memory")))))))

(deftest test-build-system-blocks-includes-memories
  (let [tmp (str (System/getProperty "java.io.tmpdir") "/syl-test-sysprompt-mem-" (System/currentTimeMillis))
        _ (alter-var-root #'memory-dir (constantly tmp))]
    (try
      (memory-write "test-key" "Test memory content")
      (let [blocks (build-system-blocks "workspace ctx")]
        (is (str/includes? (:text (first blocks)) "PERSISTENT MEMORIES"))
        (is (str/includes? (:text (first blocks)) "Test memory content")))
      (finally
        (doseq [f (.listFiles (java.io.File. tmp))]
          (.delete f))
        (.delete (java.io.File. tmp))
        (alter-var-root #'memory-dir (constantly (str project-dir "/.syl/memory")))))))

;; ============================================================
;; 12. Bedrock Backend
;; ============================================================

(deftest test-parse-cli-args-backend
  (is (= {:backend "bedrock"}
         (parse-cli-args ["--backend" "bedrock"])))
  (is (= {:backend "bedrock" :model "claude-opus-4-6"}
         (parse-cli-args ["--backend" "bedrock" "--model" "claude-opus-4-6"]))))

(deftest test-bedrock-model-id-known
  (is (= "anthropic.claude-sonnet-4-6-20250514-v1:0" (bedrock-model-id "claude-sonnet-4-6")))
  (is (= "anthropic.claude-haiku-4-5-20251001-v1:0"  (bedrock-model-id "claude-haiku-4-5")))
  (is (= "anthropic.claude-opus-4-6-20250514-v1:0"   (bedrock-model-id "claude-opus-4-6"))))

(deftest test-bedrock-model-id-passthrough
  (testing "unknown model names pass through unchanged"
    (is (= "anthropic.claude-sonnet-4-6-20250514-v1:0"
           (bedrock-model-id "anthropic.claude-sonnet-4-6-20250514-v1:0")))
    (is (= "some-custom-model" (bedrock-model-id "some-custom-model")))))

(deftest test-sha256-hex
  (testing "SHA-256 of empty string matches known value"
    (is (= "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
           (sha256-hex "")))))

(deftest test-hmac-sha256
  (testing "HMAC-SHA256 produces 32-byte output"
    (let [result (hmac-sha256 (.getBytes "key" "UTF-8") "data")]
      (is (= 32 (count result)))
      (is (bytes? result)))))

(deftest test-sign-bedrock-request
  (let [headers (sign-bedrock-request {:method "POST"
                                       :uri "/model/test/invoke"
                                       :body "{\"test\":true}"
                                       :region "us-east-1"
                                       :access-key "AKIAIOSFODNN7EXAMPLE"
                                       :secret-key "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
                                       :session-token nil})]
    (testing "returns authorization header"
      (is (contains? headers "authorization"))
      (is (str/starts-with? (get headers "authorization") "AWS4-HMAC-SHA256")))
    (testing "includes required headers"
      (is (contains? headers "x-amz-date"))
      (is (contains? headers "x-amz-content-sha256"))
      (is (contains? headers "host"))
      (is (str/includes? (get headers "host") "bedrock-runtime")))))

(deftest test-sign-bedrock-request-with-session-token
  (let [headers (sign-bedrock-request {:method "POST"
                                       :uri "/model/test/invoke"
                                       :body "{}"
                                       :region "us-west-2"
                                       :access-key "AKID"
                                       :secret-key "SECRET"
                                       :session-token "SESSION123"})]
    (testing "includes security token when provided"
      (is (= "SESSION123" (get headers "x-amz-security-token")))
      (is (str/includes? (get headers "authorization") "x-amz-security-token")))))

(deftest test-parse-eventstream-headers
  (testing "parses string headers (type 7)"
    (let [;; Build a header: name=":event-type", type=7, value="chunk"
          baos (java.io.ByteArrayOutputStream.)
          dos  (java.io.DataOutputStream. baos)
          name-bytes (.getBytes ":event-type" "UTF-8")
          val-bytes  (.getBytes "chunk" "UTF-8")]
      (.writeByte dos (count name-bytes))
      (.write dos name-bytes)
      (.writeByte dos 7) ;; string type
      (.writeShort dos (count val-bytes))
      (.write dos val-bytes)
      (let [headers (parse-eventstream-headers (.toByteArray baos))]
        (is (= "chunk" (get headers ":event-type"))))))

  (testing "returns empty map for empty bytes"
    (is (= {} (parse-eventstream-headers (byte-array 0))))))

(deftest test-parse-bedrock-stream
  (testing "parses EventStream frames and extracts Anthropic events"
    (let [events (atom [])
          ;; Build a single EventStream frame with a "chunk" event
          anthropic-event (json/generate-string {:type "message_start"
                                                  :message {:usage {:input_tokens 100}}})
          b64-event (.encodeToString (java.util.Base64/getEncoder)
                                     (.getBytes anthropic-event "UTF-8"))
          payload-json (json/generate-string {:bytes b64-event})
          payload-bytes (.getBytes payload-json "UTF-8")
          ;; Build headers: :message-type=event, :event-type=chunk
          hdr-baos (java.io.ByteArrayOutputStream.)
          hdr-dos  (java.io.DataOutputStream. hdr-baos)]
      ;; Header 1: :message-type = "event"
      (let [n (.getBytes ":message-type" "UTF-8")
            v (.getBytes "event" "UTF-8")]
        (.writeByte hdr-dos (count n)) (.write hdr-dos n)
        (.writeByte hdr-dos 7) (.writeShort hdr-dos (count v)) (.write hdr-dos v))
      ;; Header 2: :event-type = "chunk"
      (let [n (.getBytes ":event-type" "UTF-8")
            v (.getBytes "chunk" "UTF-8")]
        (.writeByte hdr-dos (count n)) (.write hdr-dos n)
        (.writeByte hdr-dos 7) (.writeShort hdr-dos (count v)) (.write hdr-dos v))
      (let [header-bytes (.toByteArray hdr-baos)
            total-len (+ 16 (count header-bytes) (count payload-bytes))
            frame-baos (java.io.ByteArrayOutputStream.)
            frame-dos  (java.io.DataOutputStream. frame-baos)]
        (.writeInt frame-dos total-len)
        (.writeInt frame-dos (count header-bytes))
        (.writeInt frame-dos 0)  ;; prelude CRC
        (.write frame-dos header-bytes)
        (.write frame-dos payload-bytes)
        (.writeInt frame-dos 0)  ;; message CRC
        (parse-bedrock-stream (ByteArrayInputStream. (.toByteArray frame-baos))
                              (fn [data] (swap! events conj data)))
        (is (= 1 (count @events)))
        (is (= "message_start" (:type (first @events))))
        (is (= 100 (-> @events first :message :usage :input_tokens)))))))

(deftest test-make-stream-handler
  (let [{:keys [on-event build-result]} (make-stream-handler)]
    (testing "handles message_start"
      (on-event {:type "message_start" :message {:usage {:input_tokens 50 :output_tokens 0}}}))
    (testing "handles text content block"
      (on-event {:type "content_block_start" :index 0 :content_block {:type "text" :text ""}})
      (on-event {:type "content_block_delta" :index 0 :delta {:type "text_delta" :text "Hello"}})
      (on-event {:type "content_block_delta" :index 0 :delta {:type "text_delta" :text " world"}})
      (on-event {:type "content_block_stop" :index 0})
      (on-event {:type "message_delta" :delta {:stop_reason "end_turn"} :usage {:output_tokens 10}}))
    (let [result (build-result)]
      (is (= "text" (:type result)))
      (is (= "Hello world" (:text result)))
      (is (= 1 (count (:raw-model-content result)))))))

;; ============================================================
;; Run all tests
;; ============================================================

(let [{:keys [fail error]} (run-tests)]
  (System/exit (if (zero? (+ fail error)) 0 1)))
