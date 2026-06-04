(use-modules (guix derivations) (guix store))

(with-store %store
  (let ((drv (derivation %store "minimal" "/bin/sh" 
                         '("-c" "echo 'Success' > $out")
                         #:env-vars '(("PATH" . "/bin"))
                         #:system "x86_64-linux")))
    (format #t "~a\n" (derivation-file-name drv))))
