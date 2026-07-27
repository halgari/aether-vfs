(ns aether.vfs.os.windows.section
  "Windows named file-mapping section via FFM (kernel32). Mirrors
  rust/crates/vfs-win/src/mapping.rs. Confines this namespace's restricted FFM
  calls; requires --enable-native-access (the :test alias already sets it).

  NOTE (JDK 26 interop): a downcall MethodHandle from Linker.downcallHandle is
  an @PolymorphicSignature native, so Clojure cannot call .invoke on it
  reflectively. Every downcall goes through .invokeWithArguments with a boxed
  Object[] (MemorySegment for pointers/HANDLEs, Integer for DWORD/BOOL, Long for
  SIZE_T), matching the FunctionDescriptor layouts below."
  (:import [java.lang.foreign Arena Linker Linker$Option SymbolLookup MemorySegment
            FunctionDescriptor MemoryLayout ValueLayout]
           [java.lang.invoke MethodHandle]))

(def ^:private ^Linker linker (Linker/nativeLinker))
(def ^:private ^Arena libs (Arena/ofShared))
;; Lazily resolve kernel32 so merely LOADING this namespace succeeds on
;; non-Windows (e.g. the Linux CI job running the full suite, where the
;; Windows-only tests self-skip). The lookup is forced only when create/open is
;; actually called — which is Windows-only.
(def ^:private k32 (delay (SymbolLookup/libraryLookup "kernel32.dll" libs)))

(defn- handle ^MethodHandle [sym ^FunctionDescriptor fd]
  (.downcallHandle linker (.orElseThrow (.find ^SymbolLookup @k32 sym)) fd (make-array Linker$Option 0)))

(def ^:private A ValueLayout/ADDRESS)
(def ^:private I ValueLayout/JAVA_INT)
(def ^:private L ValueLayout/JAVA_LONG)

(defn- fd
  "FunctionDescriptor with `res` return layout and `args` layouts. Builds the
  MemoryLayout[] explicitly since FunctionDescriptor/of is varargs (Clojure
  cannot reflectively match the trailing MemoryLayout... otherwise)."
  ^FunctionDescriptor [res & args]
  (FunctionDescriptor/of res (into-array MemoryLayout args)))

;; Delays (each forces @k32 on first use) so namespace load stays native-free.
(def ^:private mh-create (delay (handle "CreateFileMappingW" (fd A A A I I I A))))
(def ^:private mh-open   (delay (handle "OpenFileMappingW"   (fd A I I A))))
(def ^:private mh-map    (delay (handle "MapViewOfFile"      (fd A A I I I L))))
(def ^:private mh-unmap  (delay (handle "UnmapViewOfFile"    (fd I A))))
(def ^:private mh-close  (delay (handle "CloseHandle"        (fd I A))))

(def ^:private PAGE-READWRITE 0x04)
(def ^:private FILE-MAP-ALL 0xF001F)
(def ^:private ^MemorySegment INVALID-HANDLE (MemorySegment/ofAddress -1))

(defn- null? [^MemorySegment seg]
  (.equals MemorySegment/NULL seg))

(defn- wide ^MemorySegment [^Arena a ^String s]
  ;; NUL-terminated UTF-16LE. Arena.allocate zero-initializes, so the trailing
  ;; two bytes past the copied name are the UTF-16 NUL terminator.
  (let [bytes (.getBytes s "UTF-16LE")
        seg (.allocate a (long (+ (alength bytes) 2)))]
    (MemorySegment/copy (MemorySegment/ofArray bytes) (long 0) seg (long 0) (long (alength bytes)))
    seg))

(defn- map-view [^MemorySegment h size name]
  (let [view ^MemorySegment (.invokeWithArguments ^MethodHandle @mh-map
                              (object-array [h (int FILE-MAP-ALL) (int 0) (int 0) (long size)]))]
    (when (null? view)
      (.invokeWithArguments ^MethodHandle @mh-close (object-array [h]))
      (throw (ex-info "MapViewOfFile failed" {:name name})))
    {:handle h :segment (.reinterpret view (long size)) :size size :name name}))

(defn create [name size]
  (let [a (Arena/ofConfined)]
    (try
      (let [h ^MemorySegment (.invokeWithArguments ^MethodHandle @mh-create
                              (object-array [INVALID-HANDLE MemorySegment/NULL
                                             (int PAGE-READWRITE)
                                             (int (unsigned-bit-shift-right (long size) 32))
                                             (int (bit-and (long size) 0xffffffff))
                                             (wide a name)]))]
        (when (null? h) (throw (ex-info "CreateFileMappingW failed" {:name name})))
        (map-view h size name))
      (finally (.close a)))))

(defn open [name size]
  (let [a (Arena/ofConfined)]
    (try
      (let [h ^MemorySegment (.invokeWithArguments ^MethodHandle @mh-open
                              (object-array [(int FILE-MAP-ALL) (int 0) (wide a name)]))]
        (when (null? h) (throw (ex-info "OpenFileMappingW failed" {:name name})))
        (map-view h size name))
      (finally (.close a)))))

(defn close! [{:keys [handle segment size]}]
  ;; The reinterpreted view shares the raw base address MapViewOfFile returned,
  ;; so unmapping it targets the real mapped base.
  (.invokeWithArguments ^MethodHandle @mh-unmap (object-array [(.asSlice ^MemorySegment segment (long 0) (long size))]))
  (.invokeWithArguments ^MethodHandle @mh-close (object-array [^MemorySegment handle])))
