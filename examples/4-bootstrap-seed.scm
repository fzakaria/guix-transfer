;; Example 4 — a single bootstrap seed: the Guix bootstrap Guile.
;;
;; This exercises the trust anchor of the whole graph: an *executable*
;; `builtin:download` (translated to `builtin:fetchurl` with method=nar) plus an
;; input-addressed derivation whose build script generates a wrapper containing
;; store paths. The downloaded binaries are statically linked, so they run in
;; the Nix sandbox once paths are rewritten. No source compilation here — fast.
(use-modules (guix derivations) (guix store) (guix packages) (gnu packages bootstrap))

(with-store %store
  (let ((drv (package-derivation %store %bootstrap-guile)))
    (format #t "~a\n" (derivation-file-name drv))))
