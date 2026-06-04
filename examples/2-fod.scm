(use-modules (guix derivations) (guix store) (rnrs bytevectors) (guix base16))

(with-store %store
  (let ((drv (derivation %store "hello-source" "builtin:download" '()
                         #:env-vars '(("url" . "(\"https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz\")"))
                         #:hash (base16-string->bytevector "cf04afc05f242978a9d86171195aa04332993ba89f81d11b3273913000cc649c")
                         #:hash-algo 'sha256
                         #:system "x86_64-linux")))
    (format #t "~a\n" (derivation-file-name drv))))
