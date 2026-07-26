(ns aether.vfs.error
  "Error categories used throughout aether-vfs, in two related roles:

  1. General ex-info errors: (raise category msg) throws an ex-info tagged
     with {:aether.vfs/error category}; (category e) reads it back.
  2. VFS domain errors mapped to errno values understood by FUSE. VFS errors
     use categories: :not-found :io :permission-denied :invalid-argument
     :not-a-directory :is-a-directory :read-only :already-exists :not-empty."
  (:import (java.io FileNotFoundException IOException)
           (java.nio.file AccessDeniedException DirectoryNotEmptyException
                          FileAlreadyExistsException NoSuchFileException
                          NotDirectoryException)))

(defn raise
  "Throw an ex-info tagged with an :aether.vfs/error category."
  ([category msg]
   (raise category msg nil))
  ([category msg data]
   (throw (ex-info msg (assoc data :aether.vfs/error category)))))

(defn category
  "The :aether.vfs/error category of a throwable, or nil."
  [e]
  (:aether.vfs/error (ex-data e)))

(def ^:private errnos
  {:not-found 2          ; ENOENT
   :io 5                 ; EIO
   :permission-denied 13 ; EACCES
   :invalid-argument 22  ; EINVAL
   :not-a-directory 20   ; ENOTDIR
   :is-a-directory 21    ; EISDIR
   :read-only 30         ; EROFS
   :already-exists 17    ; EEXIST
   :not-empty 39})       ; ENOTEMPTY

(defn errno
  "The positive errno for an error category (FUSE replies expect positive
  errno values); unknown categories are EIO."
  ^long [category]
  (get errnos category 5))

(defmacro on-not-found
  "Evaluate expr; on a :not-found error evaluate fallback instead. Other
  errors propagate."
  [expr fallback]
  `(try
     ~expr
     (catch clojure.lang.ExceptionInfo e#
       (if (= :not-found (aether.vfs.error/category e#))
         ~fallback
         (throw e#)))))

(defmacro with-io
  "Run body, mapping java.io/java.nio filesystem exceptions to fs-error
  categories."
  [& body]
  `(try
     ~@body
     (catch NoSuchFileException e# (aether.vfs.error/raise :not-found (str e#)))
     (catch FileNotFoundException e# (aether.vfs.error/raise :not-found (str e#)))
     (catch AccessDeniedException e# (aether.vfs.error/raise :permission-denied (str e#)))
     (catch FileAlreadyExistsException e# (aether.vfs.error/raise :already-exists (str e#)))
     (catch DirectoryNotEmptyException e# (aether.vfs.error/raise :not-empty (str e#)))
     (catch NotDirectoryException e# (aether.vfs.error/raise :not-a-directory (str e#)))
     (catch IOException e# (aether.vfs.error/raise :io (str e#)))))
