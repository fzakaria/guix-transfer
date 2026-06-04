;; Example 6 — the milestone: GNU hello, end to end.
;;
;; `package-derivation` yields the full hello DAG (~hundreds of derivations:
;; the entire source bootstrap mes → tcc → gcc-mesboot → glibc → gcc → …  up
;; through coreutils/make/gcc and finally hello). Translating it is fast;
;; realising it rebuilds the world from source and is correspondingly slow.
(use-modules (guix derivations) (guix store) (guix packages) (gnu packages base))

(with-store %store
  (let ((drv (package-derivation %store hello)))
    (format #t "~a\n" (derivation-file-name drv))))
