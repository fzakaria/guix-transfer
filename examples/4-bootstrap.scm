(use-modules (guix derivations) (guix store) (guix packages) (gnu packages commencement))

(with-store %store
  (let ((drv (package-derivation %store (@@ (gnu packages commencement) m4-boot0))))
    (format #t "~a\n" (derivation-file-name drv))))
