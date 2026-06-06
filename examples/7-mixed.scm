(use-modules (guix derivations) (guix store))

;; A trivial Guix derivation that writes "hello" to $out.
;; This is the "Guix half" of the cross-ecosystem example:
;;   1. Build this .drv under Guix
;;   2. Translate it to Nix with guix-transfer
;;   3. A Nix derivation (see 7-mixed.sh) reads its output and appends " world"
(with-store %store
  (let ((drv (derivation %store "guix-hello" "/bin/sh"
                         '("-c" "echo -n hello > $out")
                         #:env-vars '(("PATH" . "/bin"))
                         #:system "x86_64-linux")))
    (format #t "~a\n" (derivation-file-name drv))))
