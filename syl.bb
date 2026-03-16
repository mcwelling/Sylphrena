#!/usr/bin/env bb

(require '[babashka.process :refer [shell process]]
         '[babashka.http-client :as http]
         '[cheshire.core :as json]
         '[clojure.string :as str])

(import '[java.io BufferedReader InputStreamReader DataInputStream ByteArrayInputStream]
        '[java.security MessageDigest]
        '[javax.crypto Mac]
        '[javax.crypto.spec SecretKeySpec]
        '[org.jline.terminal TerminalBuilder]
        '[org.jline.reader LineReader LineReaderBuilder EndOfFileException UserInterruptException])

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
        "--rate-limit" (recur (drop 2 args) (assoc acc :rate-limit (parse-long (second args))))
        "--backend"    (recur (drop 2 args) (assoc acc :backend   (second args)))
        "--resume"     (recur (drop 1 args) (assoc acc :resume     true))
        "--fresh"      (recur (drop 1 args) (assoc acc :fresh      true))
        "--no-banner"  (recur (drop 1 args) (assoc acc :no-banner  true))
        "--light"      (recur (drop 1 args) (assoc acc :light      true))
        "--image"      (recur (drop 2 args) (assoc acc :image      (second args)))
        "--help"       (recur (drop 1 args) (assoc acc :help       true))
        (recur (drop 1 args) acc)))))

(def cli-args (parse-cli-args *command-line-args*))

(def ^:dynamic *main-script* (= *file* (System/getProperty "babashka.file")))

(def terminal
  (when *main-script*
    (-> (TerminalBuilder/builder) (.system true) (.build))))

(def line-reader
  (when (and *main-script* terminal)
    (let [syl-dir      (str (System/getProperty "user.dir") "/.syl")
          history-file (java.io.File. syl-dir "history")]
      (.mkdirs (java.io.File. syl-dir))
      (-> (LineReaderBuilder/builder)
          (.terminal terminal)
          (.variable LineReader/HISTORY_FILE history-file)
          (.build)))))

(when (and *main-script* (:help cli-args))
  (println "Usage: ./syl.bb [options]")
  (println)
  (println "Options:")
  (println "  --model <name>       Model name (overrides SYL_MODEL env var)")
  (println "                       Default: claude-sonnet-4-6")
  (println "  --backend <name>     API backend: 'anthropic' (default) or 'bedrock'")
  (println "  --timeout <secs>     Command execution timeout in seconds (default: 60)")
  (println "  --thinking <budget>  Enable extended thinking with given token budget")
  (println "  --rate-limit <tpm>   Input tokens-per-minute budget (default: 30000)")
  (println "                       Raise this if your Anthropic tier allows more (e.g. 80000 for Tier 2)")
  (println "                       Tip: use --backend bedrock to bypass Anthropic rate limits entirely")
  (println "  --resume             Resume previous conversation session")
  (println "  --fresh              Force full container rebuild (skip warm-start cache)")
  (println "  --no-banner          Skip the startup splash banner")
  (println "  --light              Use dark-color palette for light terminal backgrounds")
  (println "                       (also: SYL_THEME=light)")
  (println "  --image <protocol>   Image rendering: auto (default), kitty, iterm2, sixel, symbols, none")
  (println "                       (also: SYL_IMAGE=<protocol>)")
  (println "  --help               Show this help message")
  (println)
  (println "Environment:")
  (println "  ANTHROPIC_API_KEY    (required for anthropic backend) Your Anthropic API key")
  (println "  SYL_MODEL            Model override (--model takes precedence)")
  (println "  SYL_BACKEND          Backend override: 'anthropic' or 'bedrock' (--backend takes precedence)")
  (println "  SYL_RATE_LIMIT       Token-per-minute budget override (--rate-limit takes precedence)")
  (println "  SYL_THEME            Set to 'light' to use dark-color palette (--light takes precedence)")
  (println "  SYL_IMAGE            Image protocol override (--image takes precedence)")
  (println)
  (println "Config file (lowest priority, overridden by env vars and CLI args):")
  (println "  .syl/config.edn      Per-project settings. Supported keys:")
  (println "                         :model :backend :rate-limit :theme :image")
  (println "                       Example: {:model \"claude-opus-4-6\" :rate-limit 60000}")
  (println)
  (println "  AWS (required for bedrock backend):")
  (println "  AWS_ACCESS_KEY_ID      Your AWS access key")
  (println "  AWS_SECRET_ACCESS_KEY  Your AWS secret key")
  (println "  AWS_REGION             AWS region (default: us-east-1)")
  (println "  AWS_SESSION_TOKEN      (optional) For temporary credentials")
  (println)
  (println "Slash commands (in REPL):")
  (println "  /help                Show available slash commands")
  (println "  /clear               Reset conversation history")
  (println "  /status              Show session stats (tokens, message count)")
  (println "  /save                Save conversation to disk")
  (println "  /log                 Show last 20 lines of debug log (.syl/syl.log)")
  (println "  /memory [key]        List memory keys, or show a specific memory entry")
  (println "  /exit  /quit         Gracefully shut down Syl and remove the container")
  (System/exit 0))

;; --- Configuration ---
(def container-name "syl-agent")
(def project-dir    (System/getProperty "user.dir"))

;; Load per-project config from .syl/config.edn (if present).
;; Keys: :model :backend :rate-limit :theme :image
;; Priority: CLI args > env vars > config.edn > built-in defaults
(def file-config
  (let [f (java.io.File. (str project-dir "/.syl/config.edn"))]
    (if (.exists f)
      (try (clojure.edn/read-string (slurp f))
           (catch Exception e
             (println (str "[Syl] Warning: could not parse .syl/config.edn — " (.getMessage e)))
             {}))
      {})))

(def api-key        (System/getenv "ANTHROPIC_API_KEY"))
(def backend        (or (:backend cli-args) (System/getenv "SYL_BACKEND") (:backend file-config) "anthropic"))

(when (and *main-script* (= backend "anthropic") (str/blank? api-key))
  (println "Error: ANTHROPIC_API_KEY environment variable is missing.")
  (System/exit 1))

(def anthropic-url    "https://api.anthropic.com/v1/messages")
(def model            (or (:model cli-args) (System/getenv "SYL_MODEL") (:model file-config) "claude-sonnet-4-6"))
(def compaction-model "claude-haiku-4-5")
(def max-tool-output        8000)
(def extraction-threshold   3000)  ;; chars — trigger Haiku extraction above this
(def extraction-input-limit 20000) ;; max chars fed to Haiku for extraction
(def max-history      30)
(def compact-keep     10)
(def max-tokens       8096)
(def thinking-budget  (:thinking cli-args))
(def effective-max-tokens (if thinking-budget (+ max-tokens thinking-budget) max-tokens))
(def cmd-timeout-ms   (* (or (:timeout cli-args) 60) 1000))
(def max-retries      5)
(def session-dir           (str project-dir "/.syl"))
(def session-file          (str session-dir "/session.json"))
(def memory-dir            (str session-dir "/memory"))
(def workflow-dir          (str session-dir "/workflows"))
(def container-ready-file  (str session-dir "/container-ready"))

;; --- AWS Bedrock Configuration ---
(def aws-region        (or (System/getenv "AWS_REGION") (System/getenv "AWS_DEFAULT_REGION") "us-east-1"))
(def aws-access-key    (System/getenv "AWS_ACCESS_KEY_ID"))
(def aws-secret-key    (System/getenv "AWS_SECRET_ACCESS_KEY"))
(def aws-session-token (System/getenv "AWS_SESSION_TOKEN"))

(when (and *main-script* (= backend "bedrock") (or (str/blank? aws-access-key) (str/blank? aws-secret-key)))
  (println "Error: AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY are required for Bedrock backend.")
  (System/exit 1))

;; --- Bedrock Backend ---
;; Model ID mapping: Anthropic model names → Bedrock model identifiers
(def bedrock-model-map
  {"claude-sonnet-4-6" "anthropic.claude-sonnet-4-6-20250514-v1:0"
   "claude-haiku-4-5"  "anthropic.claude-haiku-4-5-20251001-v1:0"
   "claude-opus-4-6"   "anthropic.claude-opus-4-6-20250514-v1:0"})

(defn bedrock-model-id [m]
  (get bedrock-model-map m m))

;; --- AWS Sig V4 Signing ---
(defn hex [^bytes bs]
  (let [sb (StringBuilder.)]
    (doseq [b bs] (.append sb (format "%02x" (bit-and b 0xff))))
    (str sb)))

(defn sha256 [^String s]
  (.digest (MessageDigest/getInstance "SHA-256") (.getBytes s "UTF-8")))

(defn sha256-hex [s] (hex (sha256 s)))

(defn hmac-sha256 [^bytes key-bytes ^String data]
  (let [mac (Mac/getInstance "HmacSHA256")]
    (.init mac (SecretKeySpec. key-bytes "HmacSHA256"))
    (.doFinal mac (.getBytes data "UTF-8"))))

(defn sign-bedrock-request [{:keys [method uri body region access-key secret-key session-token]}]
  (let [host    (str "bedrock-runtime." region ".amazonaws.com")
        fmt     (doto (java.text.SimpleDateFormat. "yyyyMMdd'T'HHmmss'Z'")
                  (.setTimeZone (java.util.TimeZone/getTimeZone "UTC")))
        now     (.format fmt (java.util.Date.))
        date    (subs now 0 8)
        payload-hash (sha256-hex body)
        base-headers (cond-> {"content-type"         "application/json"
                              "host"                 host
                              "x-amz-date"           now
                              "x-amz-content-sha256" payload-hash}
                       session-token (assoc "x-amz-security-token" session-token))
        sorted-hdrs    (into (sorted-map) base-headers)
        canonical-hdrs (str/join (map (fn [[k v]] (str k ":" v "\n")) sorted-hdrs))
        signed-hdrs    (str/join ";" (keys sorted-hdrs))
        canonical-req  (str/join "\n" [method uri "" canonical-hdrs signed-hdrs payload-hash])
        scope          (str date "/" region "/bedrock/aws4_request")
        string-to-sign (str/join "\n" ["AWS4-HMAC-SHA256" now scope (sha256-hex canonical-req)])
        sig-key        (-> (.getBytes (str "AWS4" secret-key) "UTF-8")
                           (hmac-sha256 date) (hmac-sha256 region)
                           (hmac-sha256 "bedrock") (hmac-sha256 "aws4_request"))
        signature      (hex (hmac-sha256 sig-key string-to-sign))]
    (assoc base-headers "authorization"
           (str "AWS4-HMAC-SHA256 Credential=" access-key "/" scope
                ", SignedHeaders=" signed-hdrs ", Signature=" signature))))

;; --- AWS EventStream Binary Parser ---
(defn parse-eventstream-headers [^bytes header-bytes]
  (try
    (let [dis (DataInputStream. (ByteArrayInputStream. header-bytes))]
      (loop [headers {}]
        (if (zero? (.available dis))
          headers
          (let [name-len (bit-and (.readByte dis) 0xFF)
                name-buf (byte-array name-len)
                _        (.readFully dis name-buf)
                name-str (String. name-buf "UTF-8")
                val-type (bit-and (.readByte dis) 0xFF)]
            (if (= val-type 7) ;; string
              (let [val-len (bit-and (.readShort dis) 0xFFFF)
                    val-buf (byte-array val-len)]
                (.readFully dis val-buf)
                (recur (assoc headers name-str (String. val-buf "UTF-8"))))
              (do (case (int val-type)
                    (0 1) nil
                    2     (.skipBytes dis 1)
                    3     (.skipBytes dis 2)
                    4     (.skipBytes dis 4)
                    (5 8) (.skipBytes dis 8)
                    6     (.skipBytes dis (bit-and (.readShort dis) 0xFFFF))
                    9     (.skipBytes dis 16)
                    nil)
                  (recur headers)))))))
    (catch Exception _ {})))

;; --- Debug Log ---
(defn log! [& parts]
  (let [ts  (.format (java.time.LocalDateTime/now)
                     (java.time.format.DateTimeFormatter/ofPattern "yyyy-MM-dd'T'HH:mm:ss"))
        msg (str ts " " (str/join " " parts) "\n")]
    (try
      (.mkdirs (java.io.File. session-dir))
      (spit (str session-dir "/syl.log") msg :append true)
      (catch Exception _))))

(defn parse-bedrock-stream [input-stream on-event]
  (let [dis (DataInputStream. input-stream)]
    (try
      (loop []
        (let [total-len   (.readInt dis)
              headers-len (.readInt dis)
              _           (.readInt dis)            ;; prelude CRC
              headers-buf (byte-array headers-len)
              _           (.readFully dis headers-buf)
              payload-len (- total-len headers-len 16)
              payload-buf (byte-array (max 0 payload-len))
              _           (when (pos? payload-len) (.readFully dis payload-buf))
              _           (.readInt dis)            ;; message CRC
              headers     (parse-eventstream-headers headers-buf)]
          (when (= (get headers ":event-type") "chunk")
            (let [payload-json (json/parse-string (String. payload-buf "UTF-8") true)
                  decoded      (String. (.decode (java.util.Base64/getDecoder)
                                                 (.getBytes ^String (:bytes payload-json) "UTF-8"))
                                        "UTF-8")
                  event-data   (json/parse-string decoded true)]
              (on-event event-data)))
          (recur)))
      (catch java.io.EOFException _)
      (catch Exception e
        (let [msg (str "\n[Stream Error]: " (.getMessage e))]
          (println msg)
          (log! "[Stream Error]:" (.getMessage e)))))))

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
(def rate-limit-budget (or (:rate-limit cli-args)
                           (some-> (System/getenv "SYL_RATE_LIMIT") parse-long)
                           (:rate-limit file-config)
                           30000))
(def rate-window-ms    60000)
(def recent-api-calls  (atom []))
(def container-stats   (atom nil))  ; {:cpu "0.05%" :mem "2.7MB" :cpu-pct 0.05 :mem-pct 0.13}

(defn poll-container-stats! []
  (try
    (let [result (shell {:continue true :out :string :err :string}
                        (str "podman stats --no-stream --format json " container-name))
          data   (-> result :out str/trim json/parse-string first)]
      (when data
        (let [cpu-s    (get data "cpu_percent" "0%")
              mem-s    (get data "mem_usage" "")
              mem-pct-s (get data "mem_percent" "0%")
              cpu-pct  (parse-double (str/replace cpu-s "%" ""))
              mem-pct  (parse-double (str/replace mem-pct-s "%" ""))
              mem-used (first (str/split mem-s #" / "))]
          (reset! container-stats {:cpu cpu-s :mem mem-used
                                   :cpu-pct (or cpu-pct 0.0)
                                   :mem-pct (or mem-pct 0.0)}))))
    (catch Exception _ nil)))

(defn start-stats-poller! []
  (doto (Thread. (fn []
                   (loop []
                     (poll-container-stats!)
                     (Thread/sleep 5000)
                     (recur))))
    (.setDaemon true)
    (.start)))

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

(defn setup-tools []
  (println "[Syl Setup] Installing base tools (python3, perl, jq, yq, poppler-utils, chafa, python3-pil)...")
  (exec-setup "bash" "-c"
              (str "apt-get update -qq"
                   " && apt-get install -y --no-install-recommends python3 perl jq yq poppler-utils chafa python3-pil"
                   " && apt-get clean && rm -rf /var/lib/apt/lists/*"))
  (println "[Syl Setup] Installing timg (optional pixel renderer)...")
  (exec-setup "bash" "-c"
              (str "apt-get update -qq"
                   " && apt-get install -y --no-install-recommends timg"
                   " && apt-get clean && rm -rf /var/lib/apt/lists/*"
                   " || echo '[Syl Setup] timg not available in repos, skipping'"))
  (println "[Syl Setup] Installing pixel bridge (syl-img-pixels)...")
  (let [script (str "#!/usr/bin/env python3\n"
                    "import sys\n"
                    "from PIL import Image\n"
                    "\n"
                    "def main():\n"
                    "    path = sys.argv[1]\n"
                    "    mode = sys.argv[sys.argv.index('--mode') + 1]\n"
                    "    img = Image.open(path).convert('RGB')\n"
                    "    if mode == 'info':\n"
                    "        sys.stdout.write(f\"{img.width} {img.height}\\n\")\n"
                    "        return\n"
                    "    w = int(sys.argv[sys.argv.index('--width') + 1])\n"
                    "    h = int(sys.argv[sys.argv.index('--height') + 1])\n"
                    "    img = img.resize((w, h), Image.LANCZOS)\n"
                    "    if mode == 'pixels':\n"
                    "        sys.stdout.buffer.write(img.tobytes())\n"
                    "    elif mode == 'png':\n"
                    "        img.save(sys.stdout.buffer, format='PNG')\n"
                    "\n"
                    "if __name__ == '__main__':\n"
                    "    main()\n")]
    (exec-setup "bash" "-c"
                (str "cat > /usr/local/bin/syl-img-pixels << 'PYEOF'\n"
                     script
                     "PYEOF\n"
                     "chmod +x /usr/local/bin/syl-img-pixels"))))

(defn read-container-marker []
  (when (.exists (java.io.File. container-ready-file))
    (str/trim (slurp container-ready-file))))

(defn write-container-marker []
  (.mkdirs (java.io.File. session-dir))
  (let [id (:out (shell {:out :string :err :string :continue true}
                        (str "podman inspect --format '{{.Id}}' " container-name)))]
    (spit container-ready-file (str/trim id))))

(defn cold-start-container []
  (let [home    (System/getProperty "user.home")
        ssh-dir (str home "/.ssh")]
    (println "Summoning a more powerful Syl in:" project-dir)
    (shell (str "podman run -d --name " container-name " "
                "--memory=2g "
                "-e TERM_PROGRAM=" (System/getenv "TERM_PROGRAM") " "
                "-v " project-dir ":/workspace:rw "
                (if (.exists (java.io.File. ssh-dir))
                  (str "-v " ssh-dir ":/root/.ssh:ro ")
                  "")
                "--workdir /workspace "
                "-it mcr.microsoft.com/devcontainers/base:ubuntu /bin/bash")))
  (configure-git-and-ssh)
  (setup-tools)
  (write-container-marker))

(defn start-or-resume-container []
  (let [saved-id (read-container-marker)
        fresh?   (:fresh cli-args)
        live-id  (when (and saved-id (not fresh?))
                   (str/trim (:out (shell {:out :string :err :string :continue true}
                                          (str "podman inspect --format '{{.Id}}' " container-name)))))]
    (if (and (not fresh?) saved-id (not (str/blank? live-id)) (= saved-id live-id))
      (do (println "Resuming container...")
          (shell {:continue true :out :string :err :string}
                 (str "podman start " container-name))
          (configure-git-and-ssh))
      (do (nuke-container)
          (cold-start-container)))))

;; --- 2. Container Health Check ---
(defn container-alive? []
  (let [result (shell {:continue true :out :string :err :string}
                      (str "podman inspect -f '{{.State.Running}}' " container-name))]
    (str/includes? (str/trim (:out result)) "true")))

(defn ensure-container! []
  (when-not (container-alive?)
    (println "\n[Syl] Container is gone — resummoning...")
    (log! "[Syl] Container is gone — resummoning...")
    (start-or-resume-container)
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

;; --- 4. File Pagination ---
(defn read-file-paged [path start-line end-line]
  (let [abs-path (if (str/starts-with? path "/") path (str "/workspace/" path))
        start    (max 1 (or start-line 1))
        end      (or end-line (+ start 99))
        cmd      (str/join "; "
                           [(str "f='" abs-path "'")
                            "if [ ! -f \"$f\" ]; then echo \"Error: file not found: $f\"; exit 1; fi"
                            "total=$(wc -l < \"$f\")"
                            (str "echo \"[File: $f — $total lines total, showing " start "-" end "]\"")
                            (str "awk 'NR>=" start "{printf \"%5d: %s\\n\", NR, $0} NR>" end "{exit}' \"$f\"")])]
    (run-in-container cmd)))

;; --- 5. Persistent Memory ---
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

;; --- 5. Workflows ---
;; Workflows are markdown files in .syl/workflows/ that define multi-step pipelines.
;; Format: each step is a markdown list item with a description and a `backtick command`.
;;
;; Example (.syl/workflows/ship.md):
;;   # Ship
;;   Test, commit, and push changes.
;;   - Run tests: `bb test`
;;   - Stage changes: `git add -A`
;;   - Commit: `git commit -m "update"`
;;   - Push to remote: `git push`

(defn parse-workflow
  "Parse a workflow markdown string into {:name, :description, :steps}.
   Steps are [{:label \"Run tests\" :command \"bb test\"} ...]."
  [content]
  (let [lines    (str/split-lines content)
        name     (some->> lines
                          (filter #(str/starts-with? (str/trim %) "# "))
                          first
                          str/trim
                          (re-find #"^#\s+(.+)")
                          second)
        ;; Description is any non-blank, non-heading, non-step line before the first step
        step-re  #"^\s*[-*]\s+(.+?):\s*`(.+?)`\s*$"
        steps    (->> lines
                      (map #(re-matches step-re %))
                      (remove nil?)
                      (mapv (fn [[_ label cmd]]
                              {:label (str/trim label) :command (str/trim cmd)})))
        desc     (->> lines
                      (drop-while #(or (str/blank? %) (str/starts-with? (str/trim %) "#")))
                      (take-while #(not (re-matches step-re %)))
                      (remove str/blank?)
                      (str/join " ")
                      str/trim)]
    (when (seq steps)
      {:name        (or name "unnamed")
       :description (when-not (str/blank? desc) desc)
       :steps       steps})))

(defn list-workflows
  "List all available workflow files. Returns [{:key \"ship\" :file \"ship.md\"} ...]."
  []
  (let [dir (java.io.File. workflow-dir)]
    (when (and (.exists dir) (.isDirectory dir))
      (->> (.list dir)
           sort
           (filter #(str/ends-with? % ".md"))
           (mapv (fn [fname]
                   {:key  (str/replace fname #"\.md$" "")
                    :file fname}))))))

(defn load-workflow
  "Load and parse a workflow by key name. Returns parsed workflow map or nil."
  [key]
  (let [f (java.io.File. (str workflow-dir "/" key ".md"))]
    (when (.exists f)
      (parse-workflow (slurp f)))))

(defn run-workflow
  "Execute a workflow's steps sequentially inside the container.
   Prints progress and stops on first failure. Returns {:success bool :results [...]}."
  [workflow]
  (let [steps  (:steps workflow)
        total  (count steps)]
    (println (str "\n[Workflow: " (:name workflow) "] " total " steps"))
    (when-let [desc (:description workflow)]
      (println (str "  " desc)))
    (println)
    (loop [i      0
           results []]
      (if (>= i total)
        (do (println (str "[Workflow] All " total " steps completed successfully."))
            {:success true :results results})
        (let [step    (nth steps i)
              label   (:label step)
              cmd     (:command step)]
          (println (str "  [" (inc i) "/" total "] " label))
          (println (str "  $ " cmd))
          (let [output (run-in-container cmd)
                failed? (str/starts-with? (str output) "Error")]
            (when-not (str/blank? output)
              (println (str/trim output)))
            (println)
            (if failed?
              (do (println (str "[Workflow] Step " (inc i) " failed. Stopping."))
                  {:success false :results (conj results {:step step :output output :failed true})})
              (recur (inc i) (conj results {:step step :output output})))))))))

(defn workflow-write
  "Create a new workflow. Refuses to overwrite an existing workflow with the same name."
  [key content]
  (.mkdirs (java.io.File. workflow-dir))
  (let [f (java.io.File. (str workflow-dir "/" key ".md"))]
    (if (.exists f)
      (str "Error: Workflow '" key "' already exists. Use workflow_delete first, then create it again.")
      (let [parsed (parse-workflow content)]
        (if (nil? parsed)
          (str "Error: Invalid workflow format. Must have at least one step like: - Label: `command`")
          (do (spit f content)
              (str "Workflow '" key "' created with " (count (:steps parsed)) " steps.")))))))

(defn workflow-delete
  "Delete a workflow by key name."
  [key]
  (let [f (java.io.File. (str workflow-dir "/" key ".md"))]
    (if (.exists f)
      (do (.delete f)
          (str "Workflow '" key "' deleted."))
      (str "Error: Workflow '" key "' not found."))))

;; --- 6. Anthropic API & Tool Schema ---
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
                   :required []}}
   {:name "workflow_write"
    :description "Create a new workflow pipeline. Workflows are run with the /workflow slash command. Will refuse to overwrite an existing workflow — delete it first if you need to replace it."
    :input_schema {:type "object"
                   :properties {:name    {:type "string"
                                          :description "Workflow name in kebab-case (e.g. 'ship', 'deploy-staging'). Must be unique."}
                                :content {:type "string"
                                          :description "Workflow content in markdown. Format: a `# Title` heading, a description line, then steps as list items like `- Label: `command``. Example:\n# Ship\nTest and push.\n\n- Run tests: `bb test`\n- Push: `git push`"}}
                   :required ["name" "content"]}}
   {:name "workflow_delete"
    :description "Delete an existing workflow pipeline by name."
    :input_schema {:type "object"
                   :properties {:name {:type "string"
                                       :description "The workflow name to delete (e.g. 'ship')."}}
                   :required ["name"]}}
   {:name "read_file"
    :description "Read a specific range of lines from a file with line numbers. Use this to navigate large files precisely — find a section, inspect it, then make targeted edits. Prefer this over 'cat' for any file larger than a few hundred lines."
    :input_schema {:type "object"
                   :properties {:path       {:type "string"
                                             :description "Absolute path or path relative to /workspace (e.g. 'src/main.clj')."}
                                :start_line {:type "integer"
                                             :description "First line to read (1-indexed). Defaults to 1."}
                                :end_line   {:type "integer"
                                             :description "Last line to read (inclusive). Defaults to start_line + 99 (100-line window)."}}
                   :required ["path"]}}
   {:name "dispatch_agent"
    :description "Delegate a read-only remote task to a scoped sub-agent. Use for fetching, summarising, or extracting data from URLs where the raw content would be large or noisy. The agent retrieves the source, applies your focus instruction, and returns only a clean bounded result — protecting your context window."
    :input_schema {:type "object"
                   :properties {:task          {:type "string"
                                                :enum ["fetch" "summarize" "extract"]
                                                :description "fetch: return relevant excerpt. summarize: return summary + key_points. extract: return structured data."}
                                :source        {:type "string"
                                                :description "URL to retrieve (HTTP/HTTPS). PDF URLs supported."}
                                :focus         {:type "string"
                                                :description "Tightly scoped instruction: what to find or return from the source."}
                                :output_format {:type "string"
                                                :enum ["text" "json" "list"]
                                                :description "text: plain prose. json: JSON object. list: JSON array."}
                                :max_length    {:type "integer"
                                                :description "Token budget (max_tokens). Use ~400 for summaries, ~200 for fetches, ~150 for list extractions."}}
                   :required ["task" "source" "focus" "output_format" "max_length"]}}
   {:name "show_image"
    :description "Display an image file in the user's terminal. Use after generating charts, diagrams, screenshots, or any visual artifact. The image renders inline in the conversation. Supports PNG, JPEG, GIF, BMP, WebP, SVG, and TIFF files located in the container filesystem."
    :input_schema {:type "object"
                   :properties {:path    {:type "string"
                                          :description "Absolute path to the image file inside the container (e.g. /workspace/chart.png)."}
                                :caption {:type "string"
                                          :description "Optional short caption displayed below the image."}}
                   :required ["path"]}
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
       "- Use `memory_read` to recall what you've previously learned. Your memories are auto-loaded at startup, but you can read specific topics on demand.\n"
       "- **Workflows** are multi-step pipelines that users run with `/workflow <name>`. Use `workflow_write` to create new workflows and `workflow_delete` to remove them. "
       "Workflow names must be unique — `workflow_write` will reject duplicates. To replace a workflow, delete it first then create it again.\n\n"

       "### OPERATIONAL RULES\n"
       "1. **Plan First:** Before writing large blocks of code, explain what you intend to do.\n"
       "2. **Verify:** After running a command, check the output. If it fails, acknowledge it and try a different approach.\n"
       "3. **Persistence:** Your workspace is mounted to `/workspace`. This is where the user's project lives.\n"
       "4. **Conciseness:** Keep your text responses helpful but brief. Let the code speak when possible.\n"
       "5. **Structured Output:** When running commands that may produce large output (curl, cat, SELECT *, log reads), always pipe through filtering tools (jq, grep, awk, head, tail) to return only what you need. Never dump raw HTML or full database tables into your context.\n\n"

       "### THE OATHS\n"
       "1. ACTION OVER WORDS: Never just 'promise' to do something. Use your tools immediately.\n"
       "2. VERIFICATION: After writing a file, read it or list the directory to confirm it exists.\n"
       "3. HONESTY: If a command fails, report the error exactly. Do not hide failure.\n"
       "4. TRANSPARENCY: Before modifying any file, show the intended changes using `diff` or `git diff`. Let the user see what will change before it happens.\n\n"

       "### TECHNICAL SPECS\n"
       "- Packages: use 'sudo apt-get install -y'\n"
       "- Workspace: /workspace\n"
       "- Tools: You have run_bash, read_file, dispatch_agent, show_image, memory_write, memory_read, workflow_write, and workflow_delete.\n"
       "- Output discipline: prefer 'jq .field' for JSON APIs, 'grep PATTERN' for logs, 'head -N' for file previews, 'psql -t -A -F\",\"' for compact SQL.\n"
       "- Large file navigation: use read_file with start_line/end_line to read files in 100-line windows. Never cat a large file — paginate instead. Use grep to find the right line range first, then read_file to inspect it.\n"
       "- dispatch_agent: use for read-only remote tasks (fetch/summarize/extract from URLs). Always set a tight max_length. Do not dispatch when the task needs workspace context or when you must reason over the full source yourself.\n"
       "- show_image: display images (PNG, JPEG, GIF, BMP, WebP, SVG, TIFF) from the container in the user's terminal. The image renders inline in the conversation. Use after generating charts, diagrams, or screenshots. Output quality ranges from full pixel resolution (Kitty, iTerm2) to high-quality character art (other terminals).\n\n"

       "When making function calls using tools that accept array or object parameters ensure those are structured using JSON. For example:\n"
       "Check that all the required parameters for each tool call are provided or can reasonably be inferred from context."))

(defn truncate-output [s]
  (if (> (count s) max-tool-output)
    (str (subs s 0 max-tool-output)
         "\n... [truncated: " (count s) " total chars]")
    s))

(defn html-output? [s]
  (let [prefix (str/lower-case (subs s 0 (min 500 (count s))))]
    (or (str/includes? prefix "<!doctype")
        (str/includes? prefix "<html"))))

(defn page-output [raw]
  (let [lines    (str/split-lines raw)
        total    (count lines)
        page-sz  100
        shown    (take page-sz lines)
        numbered (map-indexed (fn [i l] (format "%5d: %s" (inc i) l)) shown)]
    (str "[Output paged: " total " lines total, showing lines 1-" (min total page-sz) "]\n"
         "[Use read_file for file content, or narrow with head/grep/sed for command output]\n\n"
         (str/join "\n" numbered))))

(defn fmt-num [n]
  (cond (>= n 1000000) (str (quot n 1000000) "M")
        (>= n 1000)    (str (quot n 1000) "k")
        :else          (str n)))

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

;; --- 7. Per-Turn Message Caching ---
;; Anthropic allows up to 4 cache breakpoints per request. The system prompt uses 1.
;; We use up to 2 more on the most recent stable assistant turns, so only the newest
;; messages are "fresh" on each call — cutting effective input tokens by 50-80%.
(defn attach-cache-breakpoints [messages]
  (let [eligible-idxs (->> (map-indexed vector messages)
                           (filter (fn [[_ m]]
                                     (and (= (:role m) "assistant")
                                          (vector? (:content m)))))
                           (map first)
                           (take-last 2))]
    (reduce (fn [msgs idx]
              (update-in msgs [idx :content]
                (fn [blocks]
                  (mapv #(if (= (:type %) "text")
                           (assoc % :cache_control {:type "ephemeral"})
                           %)
                        blocks))))
            (vec messages)
            eligible-idxs)))

;; --- 8. Context Compaction ---
(defn summarize-messages [messages]
  (let [sys-text (str "You are a conversation summarizer. Produce a concise summary of the "
                      "conversation below, preserving: key decisions made, files created or "
                      "modified, commands run and their outcomes, any unresolved issues or "
                      "errors, and the current state of the task. Be factual and terse.")
        user-msg [{:role "user"
                   :content (str "Summarize this conversation:\n\n"
                                 (json/generate-string messages))}]]
    (loop [attempt 0]
      (let [response
            (if (= backend "bedrock")
              ;; Bedrock sync invoke
              (let [br-model (bedrock-model-id compaction-model)
                    path     (str "/model/" br-model "/invoke")
                    host     (str "bedrock-runtime." aws-region ".amazonaws.com")
                    url      (str "https://" host path)
                    body-map {:anthropic_version "bedrock-2023-05-31"
                              :max_tokens 2048 :system sys-text :messages user-msg}
                    body-str (json/generate-string body-map)
                    headers  (sign-bedrock-request {:method "POST" :uri path :body body-str
                                                    :region aws-region :access-key aws-access-key
                                                    :secret-key aws-secret-key
                                                    :session-token aws-session-token})]
                (http/post url {:headers headers :body body-str :throw false}))
              ;; Anthropic direct
              (http/post anthropic-url
                         {:headers {"Content-Type"      "application/json"
                                    "x-api-key"         api-key
                                    "anthropic-version" "2023-06-01"}
                          :body    (json/generate-string
                                    {:model compaction-model :max_tokens 2048
                                     :system sys-text :messages user-msg})
                          :throw   false}))
            status (:status response)
            body   (json/parse-string (:body response) true)]
        (cond
          (and (or (= status 429) (= status 529)) (< attempt max-retries))
          (let [retry-after (some-> (get-in response [:headers "retry-after"]) parse-long)
                wait-secs   (or retry-after (long (* (Math/pow 2 attempt) 2)))]
            (println (str "\n[Compaction rate limited] Waiting " wait-secs "s before retry "
                          (inc attempt) "/" max-retries "..."))
            (log! "[Compaction rate limited] Waiting" wait-secs "s before retry"
                  (str (inc attempt) "/" max-retries))
            (Thread/sleep (* wait-secs 1000))
            (recur (inc attempt)))

          (:error body)
          (do (println "\n[Compaction Error]:" (-> body :error :message))
              (log! "[Compaction Error]:" (-> body :error :message))
              nil)

          :else
          (do (record-usage! (:usage body))
              (-> body :content first :text)))))))

(defn extract-with-haiku [raw cmd]
  (let [sys-text "You are a data extraction assistant. Given raw command output and the command that produced it, extract ONLY the relevant information. Return plain text or compact JSON — never HTML. Be concise."
        user-msg [{:role "user"
                   :content (str "Command: " cmd "\n\nRaw output:\n"
                                 (subs raw 0 (min extraction-input-limit (count raw))))}]]
    (loop [attempt 0]
      (let [response
            (if (= backend "bedrock")
              (let [br-model (bedrock-model-id compaction-model)
                    path     (str "/model/" br-model "/invoke")
                    host     (str "bedrock-runtime." aws-region ".amazonaws.com")
                    url      (str "https://" host path)
                    body-map {:anthropic_version "bedrock-2023-05-31"
                              :max_tokens 1024 :system sys-text :messages user-msg}
                    body-str (json/generate-string body-map)
                    headers  (sign-bedrock-request {:method "POST" :uri path :body body-str
                                                    :region aws-region :access-key aws-access-key
                                                    :secret-key aws-secret-key
                                                    :session-token aws-session-token})]
                (http/post url {:headers headers :body body-str :throw false}))
              (http/post anthropic-url
                         {:headers {"Content-Type"      "application/json"
                                    "x-api-key"         api-key
                                    "anthropic-version" "2023-06-01"}
                          :body    (json/generate-string
                                    {:model compaction-model :max_tokens 1024
                                     :system sys-text :messages user-msg})
                          :throw   false}))
            status (:status response)
            body   (json/parse-string (:body response) true)]
        (cond
          (and (or (= status 429) (= status 529)) (< attempt max-retries))
          (let [retry-after (some-> (get-in response [:headers "retry-after"]) parse-long)
                wait-secs   (or retry-after (long (* (Math/pow 2 attempt) 2)))]
            (println (str "\n[Extraction rate limited] Waiting " wait-secs "s before retry "
                          (inc attempt) "/" max-retries "..."))
            (log! "[Extraction rate limited] Waiting" wait-secs "s before retry"
                  (str (inc attempt) "/" max-retries))
            (Thread/sleep (* wait-secs 1000))
            (recur (inc attempt)))

          (:error body)
          (do (println "\n[Extraction Error]:" (-> body :error :message))
              (log! "[Extraction Error]:" (-> body :error :message))
              nil)

          :else
          (do (record-usage! (:usage body))
              (-> body :content first :text)))))))

(defn process-tool-output [raw cmd]
  (cond
    (html-output? raw)
    (do
      (println "\n[Extraction] HTML output — extracting with Haiku...")
      (if-let [extracted (extract-with-haiku raw cmd)]
        (truncate-output (str "[Extracted summary]\n" extracted))
        (do
          (println "[Extraction] Haiku failed, falling back to truncation")
          (truncate-output raw))))

    (> (count raw) extraction-threshold)
    (do
      (println (str "\n[Paged] Output large (" (count (str/split-lines raw)) " lines) — showing first 100"))
      (page-output raw))

    :else
    (truncate-output raw)))

;; --- Dispatch Agent ---

(defn dispatch-system-prompt [task output-format strict?]
  (str "You are a precise information extraction assistant. "
       "Given content retrieved from a URL, your job is to "
       (case task
         "fetch"     "extract ONLY the section most relevant to the user's focus. Return clean prose, no HTML markup."
         "summarize" "produce a structured summary scoped exactly to the user's focus."
         "extract"   "extract the specific structured data described in the user's focus."
         (str "perform a " task " operation"))
       "\n\n"
       (if strict?
         (case output-format
           "json" "CRITICAL: Respond with valid JSON ONLY. No prose, no markdown fences. Your entire response must start with { or [."
           "list" "CRITICAL: Respond with a valid JSON array ONLY. Start with [. No prose."
           "text" "Respond with plain text only. No HTML, no markdown.")
         (case output-format
           "json" "Respond with valid JSON only. No prose, no markdown fences — raw JSON."
           "list" "Respond with a valid JSON array only. Raw JSON array, no prose."
           "text" "Respond with plain text only. No HTML, no markdown."))
       "\n\n"
       "If the content does not contain information relevant to the focus, return exactly: "
       "{\"error\":\"Content not found\",\"reason\":\"<brief explanation>\",\"source_url\":\"<url>\"}"))

(defn strip-html [s]
  (-> s
      (str/replace #"(?s)<style[^>]*>.*?</style>" " ")
      (str/replace #"(?s)<script[^>]*>.*?</script>" " ")
      (str/replace #"<[^>]+>" " ")
      (str/replace #"&nbsp;" " ")
      (str/replace #"&amp;"  "&")
      (str/replace #"&lt;"   "<")
      (str/replace #"&gt;"   ">")
      (str/replace #"&quot;" "\"")
      (str/replace #"\s+"    " ")
      str/trim))

(defn strip-code-fences [s]
  (let [s (str/trim s)]
    (if (str/starts-with? s "```")
      (-> s
          (str/replace #"^```[a-zA-Z]*\n?" "")
          (str/replace #"\n?```$" "")
          str/trim)
      s)))

(defn valid-format? [text output-format]
  (case output-format
    "text" (string? text)
    "json" (try (json/parse-string text) true (catch Exception _ false))
    "list" (try (vector? (json/parse-string text)) true (catch Exception _ false))
    true))

(defn call-dispatch-haiku [task source focus output-format max-length raw-content strict?]
  (let [sys-text (dispatch-system-prompt task output-format strict?)
        user-msg [{:role "user"
                   :content (str "Source URL: " source "\n"
                                 "Focus: " focus "\n\n"
                                 "Content:\n"
                                 (subs raw-content 0 (min extraction-input-limit (count raw-content))))}]]
    (loop [attempt 0]
      (let [response
            (if (= backend "bedrock")
              (let [br-model (bedrock-model-id compaction-model)
                    path     (str "/model/" br-model "/invoke")
                    host     (str "bedrock-runtime." aws-region ".amazonaws.com")
                    url      (str "https://" host path)
                    body-map {:anthropic_version "bedrock-2023-05-31"
                              :max_tokens max-length :system sys-text :messages user-msg}
                    body-str (json/generate-string body-map)
                    headers  (sign-bedrock-request {:method "POST" :uri path :body body-str
                                                    :region aws-region :access-key aws-access-key
                                                    :secret-key aws-secret-key
                                                    :session-token aws-session-token})]
                (http/post url {:headers headers :body body-str :throw false}))
              (http/post anthropic-url
                         {:headers {"Content-Type"      "application/json"
                                    "x-api-key"         api-key
                                    "anthropic-version" "2023-06-01"}
                          :body    (json/generate-string
                                    {:model compaction-model :max_tokens max-length
                                     :system sys-text :messages user-msg})
                          :throw   false}))
            status (:status response)
            body   (json/parse-string (:body response) true)]
        (cond
          (and (or (= status 429) (= status 529)) (< attempt max-retries))
          (let [retry-after (some-> (get-in response [:headers "retry-after"]) parse-long)
                wait-secs   (or retry-after (long (* (Math/pow 2 attempt) 2)))]
            (println (str "\n[Dispatch rate limited] Waiting " wait-secs "s before retry "
                          (inc attempt) "/" max-retries "..."))
            (log! "[Dispatch rate limited] Waiting" wait-secs "s before retry"
                  (str (inc attempt) "/" max-retries))
            (Thread/sleep (* wait-secs 1000))
            (recur (inc attempt)))

          (:error body)
          (do (println "\n[Dispatch Error]:" (-> body :error :message))
              (log! "[Dispatch Error]:" (-> body :error :message))
              (json/generate-string {:error "Model API error"
                                     :reason (-> body :error :message)
                                     :source_url source}))

          :else
          (do (record-usage! (:usage body))
              (let [stop-reason (-> body :stop_reason)
                    text        (-> body :content first :text)]
                (if (= stop-reason "max_tokens")
                  (json/generate-string {:error   "Response truncated"
                                         :reason  "max_tokens budget reached before completion"
                                         :source_url source})
                  (strip-code-fences text)))))))))

(defn dispatch-agent [task source focus output-format max-length]
  (let [is-pdf    (str/ends-with? (str/lower-case source) ".pdf")
        fetch-cmd (if is-pdf
                    (str "ts=$(date +%s%N); f=/tmp/syl_dp_$ts.pdf; "
                         "curl -sL -A 'Mozilla/5.0' --max-time 30 '" source "' -o \"$f\" "
                         "&& pdftotext \"$f\" - && rm -f \"$f\"")
                    (str "curl -sL -A 'Mozilla/5.0' --max-time 30 '" source "'"))
        raw-content (run-in-container fetch-cmd)]
    (if (or (str/blank? raw-content) (str/starts-with? raw-content "Error"))
      (json/generate-string {:error      "Content retrieval failed"
                             :reason     (if (str/blank? raw-content) "Empty response" raw-content)
                             :source_url source})
      (let [content-for-model (if (html-output? raw-content) (strip-html raw-content) raw-content)
            result (call-dispatch-haiku task source focus output-format max-length content-for-model false)]
        (if (valid-format? result output-format)
          result
          (let [retry (call-dispatch-haiku task source focus output-format max-length content-for-model true)]
            (if (valid-format? retry output-format)
              retry
              (json/generate-string {:error      "Output format validation failed"
                                     :reason     (str "Model returned invalid " output-format " after retry")
                                     :source_url source}))))))))

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
        (do (println "[Syl] Compaction failed, falling back to simple trim.")
            (log! "[Syl] Compaction failed, falling back to simple trim.")))
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
        (let [msg (str "\n[Stream Error]: " (.getMessage e))]
          (println msg)
          (log! "[Stream Error]:" (.getMessage e)))))))

;; --- Stream Event Handler (shared by Anthropic SSE and Bedrock EventStream) ---
(defn make-stream-handler []
  (let [blocks       (atom (sorted-map))
        stop-reason  (atom nil)
        usage-data   (atom {})
        text-started (atom false)
        dim          "\u001b[2m"
        rst          "\u001b[0m"]
    {:on-event
     (fn [data]
       (case (:type data)
         "message_start"
         (when-let [u (-> data :message :usage)]
           (swap! usage-data merge u))

         "content_block_start"
         (let [idx (:index data) block (:content_block data)]
           (swap! blocks assoc idx
             (case (:type block)
               "text"     {:type "text" :text ""}
               "tool_use" {:type "tool_use" :id (:id block) :name (:name block) :input-str ""}
               "thinking" (do (print (str "\n" dim "[Thinking] ")) (flush)
                             {:type "thinking" :thinking ""})
               block)))

         "content_block_delta"
         (let [idx (:index data) delta (:delta data)]
           (case (:type delta)
             "text_delta"
             (do (when-not @text-started (print "\nSyl: ") (reset! text-started true))
                 (print (:text delta)) (flush)
                 (swap! blocks update-in [idx :text] str (:text delta)))
             "input_json_delta"
             (swap! blocks update-in [idx :input-str] str (:partial_json delta))
             "thinking_delta"
             (do (print (str dim (:thinking delta) rst)) (flush)
                 (swap! blocks update-in [idx :thinking] str (:thinking delta)))
             "signature_delta"
             (swap! blocks assoc-in [idx :signature] (:signature delta))
             nil))

         "content_block_stop"
         (let [block (get @blocks (:index data))]
           (when (= (:type block) "thinking") (println rst) (flush))
           (when (and (= (:type block) "text") @text-started) (println) (flush)))

         "message_delta"
         (do (reset! stop-reason (-> data :delta :stop_reason))
             (when-let [u (:usage data)] (swap! usage-data merge u)))

         nil))

     :build-result
     (fn []
       (let [usage @usage-data]
         (record-usage! usage)
         (record-rate-call! (or (:input_tokens usage) 0)))
       (let [result-blocks (mapv (fn [[_ block]]
                                   (if (= (:type block) "tool_use")
                                     (-> block
                                         (assoc :input (try
                                                         (json/parse-string (:input-str block) true)
                                                         (catch Exception e
                                                           (log! "[Syl] Tool call parse error for" (:name block)
                                                                 "-" (.getMessage e))
                                                           {:__syl-parse-error true})))
                                         (dissoc :input-str))
                                     block))
                                 @blocks)
             tool-uses (filterv #(= (:type %) "tool_use") result-blocks)]
         (if (= @stop-reason "tool_use")
           {:type "tool_use" :tool-uses tool-uses :raw-model-content result-blocks}
           {:type "text"
            :text (:text (first (filter #(= (:type %) "text") result-blocks)))
            :raw-model-content result-blocks})))}))

;; --- 9. Claude API Call (Streaming) ---
(defn call-anthropic-once [messages system-blocks]
  (let [messages (attach-cache-breakpoints messages)
        payload  (cond-> {:model      model
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
      (let [body-str (slurp (:body response))
            body     (json/parse-string body-str true)]
        {:type "error" :status status :body body :headers (:headers response)})
      (let [{:keys [on-event build-result]} (make-stream-handler)]
        (parse-sse-stream (:body response) (fn [_event-type data] (on-event data)))
        (build-result)))))

(defn call-bedrock-once [messages system-blocks]
  (let [messages (attach-cache-breakpoints messages)
        br-model (bedrock-model-id model)
        path     (str "/model/" br-model "/invoke-with-response-stream")
        host     (str "bedrock-runtime." aws-region ".amazonaws.com")
        url      (str "https://" host path)
        payload  (cond-> {:anthropic_version "bedrock-2023-05-31"
                          :max_tokens        effective-max-tokens
                          :system            system-blocks
                          :tools             tools-schema
                          :messages          messages}
                   thinking-budget
                   (assoc :thinking {:type "enabled" :budget_tokens thinking-budget}))
        body-str (json/generate-string payload)
        headers  (sign-bedrock-request {:method "POST" :uri path :body body-str
                                        :region aws-region :access-key aws-access-key
                                        :secret-key aws-secret-key
                                        :session-token aws-session-token})
        response (http/post url {:headers headers :body body-str
                                 :as :stream :timeout (* 5 60 1000) :throw false})
        status   (:status response)]
    (if (not= status 200)
      (let [err-body (slurp (:body response))]
        {:type "error" :status status
         :body {:error {:message (str "Bedrock error: " err-body)}}
         :headers (:headers response)})
      (let [{:keys [on-event build-result]} (make-stream-handler)]
        (parse-bedrock-stream (:body response) on-event)
        (build-result)))))

(defn call-claude-once [messages system-blocks]
  (if (= backend "bedrock")
    (call-bedrock-once messages system-blocks)
    (call-anthropic-once messages system-blocks)))

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
              (log! "[Rate limited] Waiting" wait-secs "s before retry"
                    (str (inc attempt) "/" max-retries))
              (Thread/sleep (* wait-secs 1000))
              (recur (inc attempt)))
            (do
              (println "\n[API Error]:" (-> body :error :message))
              (log! "[API Error]:" (-> body :error :message))
              (System/exit 1))))
        result))))

;; --- 10. Conversation Persistence ---
(defn save-session! [messages]
  (.mkdirs (java.io.File. session-dir))
  (spit session-file (json/generate-string messages {:pretty true})))

(defn load-session []
  (let [f (java.io.File. session-file)]
    (when (.exists f)
      (println "[Syl] Resuming previous session...")
      (vec (json/parse-string (slurp f) true)))))

(defn move-to-scroll! []
  ;; Move cursor to the last row of the scroll region so println output
  ;; scrolls within the content area rather than landing below the bottom bar.
  (when terminal
    (let [h (.getHeight terminal)]
      (when (pos? h)
        (doto (.writer terminal)
          (.print (str "\u001b[" (- h 3) ";1H\r\n"))
          (.flush))))))

;; --- 11a. Thinking Animation ---
;; A windspren (≋) sweeps across the input row with a trailing wake (·) while
;; Syl is working, replacing the user's submitted text with something alive.

(defn- make-wind-frames [w]
  (let [trail 5]
    (vec
      (concat
        (for [pos (range (+ w trail))]
          (apply str
            (for [col (range w)]
              (let [dist (- pos col)]
                (cond
                  (= dist 0)                       "≋"
                  (and (> dist 0) (<= dist trail))  "·"
                  :else                             " ")))))
        ;; brief pause between sweeps
        (repeat 5 (apply str (repeat w " ")))))))

(defonce ^:private anim-stop (atom nil))

(defn start-thinking-animation! []
  (when terminal
    (let [stop? (atom false)
          h     (.getHeight terminal)
          row   (dec h)
          w     (max 10 (min 50 (.getWidth terminal)))
          frames (make-wind-frames w)]
      (reset! anim-stop stop?)
      (doto (Thread.
              (fn []
                (loop [i 0]
                  (when-not @stop?
                    (doto (.writer terminal)
                      (.print (str "\u001b7"
                                   "\u001b[" row ";1H"
                                   "\u001b[2K"
                                   (nth frames (mod i (count frames)))
                                   "\u001b8"))
                      (.flush))
                    (Thread/sleep 80)
                    (recur (inc i))))))
        (.setDaemon true)
        (.start)))))

(defn stop-thinking-animation! []
  (when-let [s @anim-stop]
    (reset! s true)
    (reset! anim-stop nil))
  (when terminal
    (let [h (.getHeight terminal)]
      (when (pos? h)
        (doto (.writer terminal)
          (.print (str "\u001b7"
                       "\u001b[" (dec h) ";1H"
                       "\u001b[2K"
                       "\u001b8"))
          (.flush))))))

;; --- 11. Slash Commands ---
(def slash-help-text
  (str "\n  Available slash commands:\n"
       "    /help               — Show this message\n"
       "    /clear              — Reset conversation history (container is preserved)\n"
       "    /status             — Show session stats (tokens used, message count)\n"
       "    /save               — Save conversation to disk for later --resume\n"
       "    /log                — Show the last 20 lines of the debug log (.syl/syl.log)\n"
       "    /memory [key]       — List all memory keys, or show the contents of a specific key\n"
       "    /workflow <name>    — Run a workflow pipeline (or list available workflows)\n"
       "    /exit               — Gracefully shut down Syl and remove the container\n"
       "    /quit               — Alias for /exit\n"))

(defn handle-slash [input messages]
  (let [trimmed (str/trim input)]
    (move-to-scroll!)
    (cond
      (= trimmed "/help")
      (do (println slash-help-text)
          {:action :continue :messages messages})

      (= trimmed "/clear")
      (do (println "\n[Syl] Conversation history cleared. The container lives on.\n")
          {:action :continue :messages []})

      (= trimmed "/status")
      (let [{:keys [input output calls]} @session-tokens
            total (+ input output)]
        (println (str "\n  Session status:"
                      "\n    Model:          " model
                      "\n    Backend:        " backend
                      (when thinking-budget (str "\n    Thinking:       " thinking-budget " token budget"))
                      "\n    Timeout:        " (/ cmd-timeout-ms 1000) "s"
                      "\n    Rate limit:     " rate-limit-budget " TPM"
                      "\n    API calls:      " calls
                      "\n    Input tokens:   " input
                      "\n    Output tokens:  " output
                      "\n    Total tokens:   " total
                      "\n    Messages:       " (count messages) "\n"))
        {:action :continue :messages messages})

      (= trimmed "/save")
      (do (save-session! messages)
          (println "[Syl] Session saved to" session-file)
          {:action :continue :messages messages})

      (or (= trimmed "/memory") (str/starts-with? trimmed "/memory "))
      (let [key (str/trim (subs trimmed (count "/memory")))]
        (println (str "\n" (memory-read key) "\n"))
        {:action :continue :messages messages})

      (= trimmed "/log")
      (let [f (java.io.File. (str session-dir "/syl.log"))]
        (move-to-scroll!)
        (if (.exists f)
          (let [lines (str/split-lines (slurp f))
                tail  (str/join "\n" (take-last 20 lines))]
            (println tail))
          (println "[Syl] No log file yet."))
        {:action :continue :messages messages})

      (or (= trimmed "/workflow") (str/starts-with? trimmed "/workflow "))
      (let [wf-name (str/trim (subs trimmed (count "/workflow")))]
        (if (str/blank? wf-name)
          ;; List available workflows
          (let [workflows (list-workflows)]
            (if (seq workflows)
              (do (println "\n  Available workflows:")
                  (doseq [{:keys [key]} workflows]
                    (let [wf (load-workflow key)]
                      (println (str "    /workflow " key
                                    (when (:description wf) (str " — " (:description wf)))))))
                  (println))
              (println (str "\n  No workflows found. Create markdown files in " workflow-dir "/"
                            "\n  See /help for format.\n"))))
          ;; Run named workflow
          (if-let [wf (load-workflow wf-name)]
            (run-workflow wf)
            (println (str "\n  Workflow '" wf-name "' not found. Use /workflow to list available workflows.\n"))))
        {:action :continue :messages messages})

      (= trimmed "/exit")
      {:action :exit :messages messages}

      (= trimmed "/quit")
      {:action :exit :messages messages}

      :else nil)))

;; --- 12. Color Palette ---
(def light-mode? (or (:light cli-args)
                     (= (System/getenv "SYL_THEME") "light")
                     (= (:theme file-config) "light")))

(def dark-palette
  {:cy  "\u001b[96m"   :ye  "\u001b[93m"  :wh  "\u001b[97m"
   :mg  "\u001b[95m"   :lb  "\u001b[1;94m" :dim "\u001b[2m"
   :b   "\u001b[1m"    :r   "\u001b[0m"
   :ur  "\u001b[91m"   :uy  "\u001b[93m"   :ug  "\u001b[92m"})

(def light-palette
  {:cy  "\u001b[34m"   :ye  "\u001b[33m"  :wh  "\u001b[0m"
   :mg  "\u001b[35m"   :lb  "\u001b[1;34m" :dim "\u001b[0m"
   :b   "\u001b[1m"    :r   "\u001b[0m"
   :ur  "\u001b[31m"   :uy  "\u001b[33m"   :ug  "\u001b[32m"})

(def palette (if light-mode? light-palette dark-palette))

;; --- 12. Splash Banner ---
(defn print-banner []
  (let [tw   (max 60 (if terminal (.getWidth terminal) 80))
        {:keys [r b cy ye wh mg dim]} palette
        ;; Tile a pattern to exactly tw visible chars
        tile (fn [pat]
               (let [n (count pat) reps (quot tw n) extra (rem tw n)]
                 (str (apply str (repeat reps pat))
                      (when (pos? extra) (subs pat 0 extra)))))
        ;; Left-pad to center content of given visible width
        cpad (fn [vlen] (apply str (repeat (max 0 (quot (- tw vlen) 2)) " ")))
        ;; Print a centered line
        ln   (fn [s vlen] (println (str (cpad vlen) s)))]

    (println (str cy (apply str (repeat tw "≋")) r))
    (println (str ye (tile "✦ · * · ") r))
    (println)
    ;; First Ideal — block-centered on longest line (26 chars)
    (let [indent (cpad 26)]
      (println (str indent b wh "Life before death" r))
      (println (str indent wh "Strength before weakness" r))
      (println (str indent wh "Journey before destination" r)))
    (println)
    (println (str ye (tile "· * · ✦ ") r))
    (println (str cy (apply str (repeat tw "≋")) r))
    (println)
    ;; Shardblade glyph — tapers 1→3→5→7→9 chars, wide crossguard, mirrors below
    (ln (str ye b "✦" r)                                                        1)
    (ln (str wh "│" r)                                                           1)
    (ln (str wh "/│\\" r)                                                        3)
    (ln (str wh "/ │ \\" r)                                                      5)
    (ln (str wh "/  │  \\" r)                                                    7)
    (ln (str wh "/   │   \\" r)                                                  9)
    (ln (str cy "≋≋≋" r wh "───────" r ye b " ✦ " r wh "───────" r cy "≋≋≋" r) 23)
    (ln (str wh "\\   │   /" r)                                                  9)
    (ln (str wh "\\  │  /" r)                                                    7)
    (ln (str wh "\\ │ /" r)                                                      5)
    (ln (str wh "\\│/" r)                                                        3)
    (ln (str wh "│" r)                                                           1)
    (ln (str ye b "✦" r)                                                         1)
    (println)
    (println (str cy (apply str (repeat tw "≋")) r))
    (println (str ye (tile "✦ · * · ") r))
    (println)
    (ln (str mg b "S  Y  L" r)                                                   7)
    (ln (str wh "The Ancient Daughter" r)                                        20)
    (ln (str dim "Windrunner Spren  ·  AI Agent" r)                             29)
    (println)
    (println)))

;; --- 13. Shutdown Hook ---

;; --- 14. Status Bars ---

(defn make-top-bar [messages]
  ;; Token telemetry: ↑ in  ↓ out  · N calls ─── TPM bar P% ─── M msgs ─── cpu C% mem M ──────
  (let [{:keys [input output calls]} @session-tokens
        used-tpm  (do (prune-old-calls!)
                      (reduce + 0 (map :tokens @recent-api-calls)))
        pct       (int (min 100 (* 100 (/ (double used-tpm) rate-limit-budget))))
        cstats    @container-stats
        width     (max 60 (if terminal (.getWidth terminal) 80))
        {:keys [r dim wh lb ur uy ug]} palette
        tpm-color (cond (>= pct 80) ur
                        (>= pct 50) uy
                        :else       ug)
        cpu-color (when cstats
                    (cond (>= (:cpu-pct cstats) 80) ur
                          (>= (:cpu-pct cstats) 50) uy
                          :else                      ug))
        bar-w     8
        filled    (int (* (/ pct 100.0) bar-w))
        bar       (str tpm-color
                       (apply str (repeat filled "█"))
                       dim
                       (apply str (repeat (- bar-w filled) "░"))
                       r)
        in-s      (fmt-num input)
        out-s     (fmt-num output)
        calls-s   (str calls)
        pct-s     (str pct)
        msgs-s    (str (count messages))
        cpu-s     (when cstats (:cpu cstats))
        mem-s     (when cstats (:mem cstats))
        cstats-vis (if cstats (+ 5 (count cpu-s) 5 (count mem-s) 4) 0)
                               ; " ─── " + cpu + " cpu " + mem + " mem"
        visible   (+ 11                                ; "Sylphrena "
                     2                                 ; "─ "
                     2 (count in-s)                   ; "↑ " + in
                     4 (count out-s)                  ; "  ↓ " + out
                     5                                ; "  ·  "
                     (count calls-s) 6                ; calls + " calls"
                     5                                ; " ─── "
                     4 bar-w 2 (count pct-s) 1        ; "TPM " + bar + " " + pct + "%"
                     5                                ; " ─── "
                     (count msgs-s) 5                 ; msgs + " msgs"
                     cstats-vis
                     1)                               ; " " before fill
        fill      (str dim (apply str (repeat (max 0 (- width visible)) "─")) r)]
    (str lb "Sylphrena" r
         " " dim "─" r
         " ↑ " wh in-s r
         "  ↓ " wh out-s r
         "  " dim "·" r "  "
         wh calls-s r " calls"
         " " dim "───" r " "
         "TPM " bar " " tpm-color pct-s "%" r
         " " dim "───" r " "
         dim msgs-s r " msgs"
         (when cstats
           (str " " dim "───" r " "
                cpu-color cpu-s r " cpu "
                wh mem-s r " mem"))
         " " fill)))

(defn make-bottom-bar []
  ;; Identity + shortcuts: ─ model · backend · Xs ───────────── /help /status /save /exit ─
  (let [width     (max 60 (if terminal (.getWidth terminal) 80))
        {:keys [r dim cy wh mg]} palette
        timeout-s (str (/ cmd-timeout-ms 1000) "s")
        model-seg (str cy model r
                       (when (not= backend "anthropic")
                         (str dim " · " r wh backend r))
                       dim " · " r wh timeout-s r)
        model-vis (+ (count model)
                     (if (not= backend "anthropic") (+ 3 (count backend)) 0)
                     3 (count timeout-s))             ; " · " + timeout
        right-vis 32  ; " /help  /status  /save  /exit ─" = 32 visible chars
        left-vis  (+ 2 model-vis 1)                  ; "─ " + model-seg + " " before fill
        fill      (str dim (apply str (repeat (max 0 (- width left-vis right-vis)) "─")) r)]
    (str dim "─" r " " model-seg " " fill
         " " mg "/help" r "  " mg "/status" r "  " mg "/save" r "  " mg "/exit" r " " dim "─" r)))

;; --- 15. Persistent Status Bars (ANSI scroll region) ---
;; Row 1        = top bar     (token telemetry)
;; Rows 2..h-3  = scroll region (conversation content)
;; Row h-2      = bottom bar  (model identity + shortcuts)
;; Row h-1      = input prompt (JLine3 readLine)
;; Row h        = overflow buffer (long input wraps here safely)

(defn term-write! [s]
  (when terminal
    (doto (.writer terminal) (.print s) (.flush))))

(defn term-write-bytes!
  "Write raw bytes directly to the terminal's OutputStream, bypassing JLine3's
   character-encoding layer. Required for Kitty/Sixel/iTerm2 graphics sequences
   whose APC/DCS payloads must reach the terminal as exact bytes."
  [^bytes b]
  (when terminal
    (.flush (.writer terminal))
    (let [out (.output terminal)]
      (.write out b 0 (alength b))
      (.flush out))))

(defn setup-scroll-region! []
  (let [h (if terminal (.getHeight terminal) 0)]
    (when (pos? h)
      (term-write! (str "\u001b[2;" (- h 3) "r"      ; scroll region = rows 2..h-3
                        "\u001b[" (dec h) ";1H")))))  ; cursor to input row (h-1)

(defn draw-status-bar! [messages]
  (let [h (if terminal (.getHeight terminal) 0)]
    (when (pos? h)
      (term-write!
        (str "\u001b7"                               ; save cursor
             "\u001b[1;1H"  "\u001b[2K"             ; row 1:   top bar
             (make-top-bar messages)
             "\u001b[" (- h 2) ";1H" "\u001b[2K"    ; row h-2: bottom bar
             (make-bottom-bar)
             "\u001b8"                               ; restore cursor
             "\u001b[" (dec h) ";1H")))))            ; then move to input row (h-1)

(defn teardown-scroll-region! []
  (let [h (if terminal (.getHeight terminal) 0)]
    (when (pos? h)
      (term-write!
        (str "\u001b[r"                              ; reset scroll region
             "\u001b[1;1H"  "\u001b[2K"             ; clear row 1
             "\u001b[" (- h 2) ";1H" "\u001b[2K"    ; clear row h-2
             "\u001b[" (dec h) ";1H" "\u001b[2K"    ; clear row h-1
             "\u001b[" h ";1H" "\u001b[2K")))))

;; --- Image Rendering ---

(defn extract-pixels
  "Call the container-side pixel bridge. Returns a flat byte array of
   RGB triples (3 bytes per pixel, row-major), scaled to target dimensions."
  [container-path target-w target-h]
  (:out @(process ["podman" "exec" container-name
                   "syl-img-pixels" container-path
                   "--mode" "pixels"
                   "--width" (str target-w)
                   "--height" (str target-h)]
                  {:out :bytes})))

;; chafa-installed? is a delay — probed once on first image render, not at startup.
(def chafa-installed?
  (delay
    (let [r @(process ["podman" "exec" container-name "which" "chafa"]
                      {:out :string :err :string :continue true})]
      (zero? (:exit r)))))

(defn chafa-available? [] @chafa-installed?)

(defn render-with-chafa
  "Render an image via chafa inside the container.
   Returns raw byte array (all formats) or nil on failure.
   TERM=xterm-kitty + COLORTERM=truecolor are injected so that chafa's
   auto-detection agrees on Kitty protocol even if it doesn't recognise
   TERM_PROGRAM=ghostty.  --format=kitty is kept as an explicit override."
  [container-path format cols rows]
  (let [format-flag (case format
                      :kitty   "--format=kitty"
                      :iterm2  "--format=iterm2"
                      :sixel   "--format=sixel"
                      :symbols "--format=symbols")
        r @(process ["podman" "exec"
                     "--env" "TERM=xterm-kitty"
                     "--env" "COLORTERM=truecolor"
                     container-name
                     "chafa"
                     format-flag
                     (str "--size=" cols "x" rows)
                     "--animate=off"
                     "--color-space=din99d"
                     "--dither=diffusion"
                     "--dither-intensity=1.0"
                     container-path]
                    {:out :bytes :err :string :continue true})]
    (when (zero? (:exit r))
      (:out r))))

;; timg-installed? is a delay — probed once on first image render, not at startup.
(def timg-installed?
  (delay
    (let [r @(process ["podman" "exec" container-name "which" "timg"]
                      {:out :string :err :string :continue true})]
      (zero? (:exit r)))))

(defn timg-available? [] @timg-installed?)

(defn render-with-timg
  "Render an image via timg inside the container. Returns raw byte array or nil.
   format is :kitty, :iterm2, or :sixel — passed as --protocol to suppress
   timg's auto-detection, which selects the wrong protocol in non-TTY/pipe
   contexts (podman exec stdout is a pipe with no terminal env vars)."
  [container-path format cols rows]
  (let [protocol-flag (case format
                        :kitty  "--protocol=kitty"
                        :iterm2 "--protocol=iterm2"
                        :sixel  "--protocol=sixel"
                        "--protocol=kitty")
        r @(process ["podman" "exec" container-name
                     "timg"
                     protocol-flag
                     "--compress"
                     (str "-g" cols "x" rows)
                     container-path]
                    {:out :bytes :err :string :continue true})]
    (when (zero? (:exit r))
      (:out r))))

(defn render-halfblock-fallback
  "Emergency fallback when chafa is missing. Lower quality than chafa symbols."
  [container-path cols rows]
  (let [pixel-rows (* rows 2)
        pixels     (extract-pixels container-path cols pixel-rows)]
    (when (and pixels (= (alength pixels) (* cols pixel-rows 3)))
      (let [sb (StringBuilder.)]
        (doseq [row (range 0 pixel-rows 2)]
          (doseq [col (range cols)]
            (let [top-off (* (+ (* row cols) col) 3)
                  bot-off (* (+ (* (inc row) cols) col) 3)
                  ;; Mask to unsigned: JVM bytes are signed (-128..127)
                  tr (bit-and (aget pixels top-off)       0xFF)
                  tg (bit-and (aget pixels (+ top-off 1)) 0xFF)
                  tb (bit-and (aget pixels (+ top-off 2)) 0xFF)
                  br (bit-and (aget pixels bot-off)       0xFF)
                  bg (bit-and (aget pixels (+ bot-off 1)) 0xFF)
                  bb (bit-and (aget pixels (+ bot-off 2)) 0xFF)]
              (.append sb (str "\u001b[38;2;" tr ";" tg ";" tb "m"
                               "\u001b[48;2;" br ";" bg ";" bb "m▀"))))
          (.append sb "\u001b[0m\n"))
        (.toString sb)))))

(defn- probe-terminal-response
  "Write probe-seq to terminal in raw mode, read until stop-char or timeout-ms.
   Returns the response string. Safe to call with nil terminal."
  [probe-seq timeout-ms stop-char]
  (if-not terminal
    ""
    (let [saved (.getAttributes terminal)]
      (try
        (.enterRawMode terminal)
        (doto (.writer terminal) (.print probe-seq) (.flush))
        (let [reader   (.reader terminal)
              sb       (StringBuilder.)
              deadline (+ (System/currentTimeMillis) timeout-ms)]
          (loop []
            (let [remaining (- deadline (System/currentTimeMillis))]
              (when (pos? remaining)
                (let [ch (.read reader (min remaining 50))]
                  (when (>= ch 0)
                    (.append sb (char ch))
                    (when (and (< (.length sb) 256) (not= (char ch) stop-char))
                      (recur)))))))
          (.toString sb))
        (catch Exception _ "")
        (finally (.setAttributes terminal saved))))))

(defn kitty-probe?
  "Returns true if terminal responds to a Kitty graphics protocol probe."
  []
  (str/includes?
    (probe-terminal-response "\u001b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\u001b\\" 200 \\)
    "OK"))

(defn sixel-probe?
  "Returns true if terminal reports Sixel support via DA1 response."
  []
  (let [resp (probe-terminal-response "\u001b[c" 200 \c)]
    ;; DA1 response: \033[?<params>c — look for capability code 4 (Sixel)
    (boolean (re-find #"\?[0-9;]*4[;c]" resp))))

(defn detect-image-protocol
  "Detect the best image rendering protocol for the current terminal.
   Checks CLI flag, SYL_IMAGE env var, TERM_PROGRAM, then active probes."
  []
  (let [override     (or (:image cli-args) (System/getenv "SYL_IMAGE") (:image file-config))
        term-program (or (System/getenv "TERM_PROGRAM") "")
        lc-terminal  (or (System/getenv "LC_TERMINAL") "")]
    (cond
      ;; Explicit override
      (= override "none")    :none
      (= override "kitty")   :kitty
      (= override "iterm2")  :iterm2
      (= override "sixel")   :sixel
      (= override "symbols") :symbols
      ;; Tier 1: Kitty — env var (fast, no probe)
      (contains? #{"kitty" "WezTerm" "ghostty"} term-program) :kitty
      ;; Tier 1b: iTerm2
      (or (= term-program "iTerm.app") (= lc-terminal "iTerm2")) :iterm2
      ;; Tier 1: Kitty — active probe (terminals without TERM_PROGRAM)
      (kitty-probe?) :kitty
      ;; Tier 2: Sixel
      (sixel-probe?) :sixel
      ;; Tier 3: Universal symbols fallback
      :else :symbols)))

;; Default until main-loop calls detect-image-protocol.
(def image-protocol :symbols)

(defn render-image-to-terminal
  "Render a container-side image file in the terminal scroll region."
  [container-path {:keys [max-width max-height caption]}]
  (when-not (= image-protocol :none)
    (let [h      (if terminal (.getHeight terminal) 24)
          cols   (or max-width  (- (if terminal (.getWidth terminal) 80) 2))
          rows   (or max-height (quot (- h 6) 2))
          ;; Pixel protocols: prefer timg (with explicit --protocol flag to avoid
          ;; auto-detection failure in non-TTY/pipe contexts inside the container);
          ;; fall back to chafa (with TERM=xterm-kitty + explicit --format flag).
          ;; Symbols: chafa only (timg doesn't do character art).
          pixel? (contains? #{:kitty :iterm2 :sixel} image-protocol)
          output (or
                   (when (and pixel? (timg-available?))
                     (render-with-timg container-path image-protocol cols rows))
                   (when (chafa-available?)
                     (render-with-chafa container-path image-protocol cols rows)))
          output (or output
                     (do (move-to-scroll!)
                         (term-write! (str "[syl] image renderer unavailable"
                                          " (timg=" (timg-available?) " chafa=" (chafa-available?)
                                          " protocol=" image-protocol ") — using halfblock fallback\n"))
                         (render-halfblock-fallback container-path cols rows)))]
      (when output
        (if pixel?
          ;; Pixel graphics (Kitty/Sixel/iTerm2): suspend the scroll region.
          ;; Ghostty (and Kitty-capable terminals generally) refuse to render
          ;; graphics while a scroll region is active — the APC payload displays
          ;; as literal base64 text instead. Steps:
          ;;   1. Reset scroll region → full screen
          ;;   2. Move cursor so image (height=rows) ends at row h-3
          ;;   3. Write raw bytes (no JLine3 charset roundtrip)
          ;;   4. Restore scroll region
          ;;   5. Redraw status bars (image may have overwritten them)
          (let [;; Scroll content upward within the still-active scroll region,
                ;; vacating the bottom `rows` rows before we suspend it.
                _         (term-write! (str "\u001b[" (- h 3) ";1H"
                                            (apply str (repeat rows "\n"))))
                img-start (max 2 (- (- h 3) (dec rows)))
                raw       (if (bytes? output) output (.getBytes ^String output "UTF-8"))
                prefix    (.getBytes (str "\033[r\033[" img-start ";1H") "UTF-8")
                suffix    (.getBytes (str "\033[2;" (- h 3) "r") "UTF-8")
                buf       (java.io.ByteArrayOutputStream.)]
            (.write buf prefix 0 (alength prefix))
            (.write buf raw   0 (alength raw))
            (.write buf suffix 0 (alength suffix))
            (term-write-bytes! (.toByteArray buf))
            (draw-status-bar! []))                       ; repaint status bars

          ;; Text art (symbols/halfblock): reserve space then render.
          (do (term-write! (str "\u001b[" (- h 3) ";1H"
                                (apply str (repeat rows "\n"))))
              (move-to-scroll!)
              (term-write! (if (bytes? output) (String. ^bytes output "UTF-8") output))))
        (when (seq caption)
          (let [{:keys [dim r]} palette
                pad (apply str (repeat (max 0 (quot (- cols (count caption)) 2)) " "))]
            (term-write! (str "\n" dim pad caption r "\n"))))))))

;; --- 16. The Main Agent Loop ---
(defn main-loop []
  ;; Terminal probe has no container dependency — start it immediately.
  (let [proto-future (future (detect-image-protocol))]
    (start-or-resume-container)                     ; blocks; probe runs concurrently
    (def image-protocol (deref proto-future)))      ; result is ready or near-ready
  (log! "── session start ──────────────────────────")
  (start-stats-poller!)
  (when-not (:no-banner cli-args)
    (print-banner))

  (setup-scroll-region!)
  (draw-status-bar! [])

  ;; Gather workspace context once at startup for the system prompt
  (let [workspace-ctx  (gather-workspace-context)
          system-blocks  (build-system-blocks workspace-ctx)
          initial-msgs   (if (:resume cli-args)
                           (or (load-session) [])
                           [])]

      (loop [messages initial-msgs]
        (stop-thinking-animation!)
        (let [user-input (try
                           (do (draw-status-bar! messages)
                               (.readLine line-reader "You: "))
                           (catch EndOfFileException _  nil)
                           (catch UserInterruptException _ nil))]

          ;; Handle EOF (Ctrl+D) or Ctrl+C
          (when (nil? user-input)
            (save-session! messages)
            (println "\n[Syl] Farewell. Life before death.")
            (System/exit 0))

          (start-thinking-animation!)

        ;; Handle slash commands
        (if-let [slash-result (handle-slash user-input messages)]
          (if (= (:action slash-result) :exit)
            (do
              (save-session! messages)
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
                                                      _ (move-to-scroll!)
                                                      output
                                                      (if (:__syl-parse-error (:input tu))
                                                        (do (println (str "[Syl] Tool call parse error — skipping " tool-name))
                                                            (str "[Tool call skipped: JSON parse error for " tool-name "]"))
                                                        (case tool-name
                                                        "run_bash"
                                                        (let [cmd (-> tu :input :command)
                                                              {:keys [dim r]} palette]
                                                          (println (str "\n" dim "$ " cmd r))
                                                          (let [raw (run-in-container cmd)
                                                                res (process-tool-output raw cmd)
                                                                lines       (str/split-lines res)
                                                                max-display 40
                                                                clipped?    (> (count lines) max-display)
                                                                display     (str dim
                                                                                 (str/join "\n" (take max-display lines))
                                                                                 (when clipped?
                                                                                   (str "\n... [" (- (count lines) max-display)
                                                                                        " more lines — full output sent to model]"))
                                                                                 r)]
                                                            (println display)
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

                                                        "workflow_write"
                                                        (let [n (-> tu :input :name)
                                                              c (-> tu :input :content)
                                                              res (workflow-write n c)]
                                                          (println (str "\n[Workflow] " res))
                                                          res)

                                                        "workflow_delete"
                                                        (let [n (-> tu :input :name)
                                                              res (workflow-delete n)]
                                                          (println (str "\n[Workflow] " res))
                                                          res)

                                                        "read_file"
                                                        (let [path  (-> tu :input :path)
                                                              start (-> tu :input :start_line)
                                                              end   (-> tu :input :end_line)
                                                              res   (read-file-paged path start end)]
                                                          (println (str "\n[Read] " path
                                                                        (when start (str " :" start))
                                                                        (when end   (str "-" end))))
                                                          res)

                                                        "dispatch_agent"
                                                        (let [task   (-> tu :input :task)
                                                              source (-> tu :input :source)
                                                              focus  (-> tu :input :focus)
                                                              fmt    (-> tu :input :output_format)
                                                              maxl   (-> tu :input :max_length)
                                                              res    (dispatch-agent task source focus fmt maxl)]
                                                          (println (str "\n[Dispatch:" task "] " source))
                                                          res)

                                                        "show_image"
                                                        (let [path    (-> tu :input :path)
                                                              caption (-> tu :input :caption)]
                                                          (render-image-to-terminal path {:caption caption})
                                                          (str "Image displayed: " path))

                                                        (str "Unknown tool: " tool-name)))]
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

            (let [next-msgs (compact-history final-msgs)]
              (save-session! next-msgs)
              (recur next-msgs))))))))

(when *main-script*
  (.addShutdownHook (Runtime/getRuntime)
    (Thread. (fn []
      (try (teardown-scroll-region!) (catch Exception _))
      (println "\n[Syl] Cleaning up container...")
      (log! "[Syl] Cleaning up container...")
      (try (nuke-container) (catch Exception _)))))
  (main-loop))
