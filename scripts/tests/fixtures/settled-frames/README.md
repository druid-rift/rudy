# Settled-frame fixtures

Two real frames from boot run `20260826T171152Z`, kept because the floor in
`boot_evidence.MIN_SETTLED_NON_BLACK_FRACTION` is only defensible against the
two cases that bracket it. Synthesised frames prove the arithmetic; these prove
the arithmetic was aimed at the right thing.

| File | Case | Non-black | What it is |
| --- | --- | ---: | --- |
| `blank-cursor-on-black.png` | `arch-ntfs-gpt` | 0.0106% | The false pass ticket 16 found — a mouse cursor on black. |
| `sparse-text-console.png` | `arch-stock-ntfs-gpt` | 1.6684% | The sparsest frame any case has legitimately passed on — a root shell. |
| `exfat-initramfs-rescue-shell.png` | `ubuntu-exfat-gpt` | — | A run that *passed* while the image sat at a BusyBox prompt. |
| `ntfs-subiquity-installer.png` | `ubuntu-ntfs-gpt` | — | The installer that pass was supposed to be distinguished from. |

The floor sits between the first two with an order of magnitude either side. The last
two are the pair the *frame-text* check exists for: both are lit, both change, and only
reading the text tells them apart. Replacing
these with frames from a later run is fine; moving the floor because a *new*
frame sits near it is not, without the reasoning in the ticket being redone.
