(use-modules (guix derivations) (guix store))

(with-store %store
  (let* ((b (derivation %store "dependency-b" "/bin/sh" 
                        '("-c" "echo 'Shared Secret' > $out")
                        #:env-vars '(("PATH" . "/bin"))
                        #:system "x86_64-linux"))
         (a (derivation %store "dependency-a" "/bin/sh" 
                        (list "-c" (format #f "read line < ~a; echo \"Captured: $line\" > $out" (derivation->output-path b)))
                        #:env-vars '(("PATH" . "/bin"))
                        #:inputs (list (list b "out"))
                        #:system "x86_64-linux")))
    (format #t "~a\n" (derivation-file-name a))))
