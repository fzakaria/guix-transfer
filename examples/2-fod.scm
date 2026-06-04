(use-modules (guix derivations) (guix store) (rnrs bytevectors) (guix base16))

(with-store %store
  (let ((drv (derivation %store "hello-source" "builtin:download" '()
                         #:env-vars '(("url" . "(\"https://ftp.gnu.org/gnu/hello/hello-2.12.tar.gz\")"))
                         #:hash (base16-string->bytevector "cf04af86dc085268c5f4470fbae49b18afbc221b78096aab842d934a76bad0ab")
                         #:hash-algo 'sha256
                         #:system "x86_64-linux")))
    (format #t "~a\n" (derivation-file-name drv))))
