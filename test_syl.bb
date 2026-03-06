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
;; 8. Slash Commands
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

;; ============================================================
;; 9. Rate Limit Budgeting
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
;; 10. System Prompt Construction
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
;; Run all tests
;; ============================================================

(let [{:keys [fail error]} (run-tests)]
  (System/exit (if (zero? (+ fail error)) 0 1)))
