# xattr compatibility patch

Source: xattr 0.2.3 from crates.io, https://github.com/Stebalien/xattr.
The original MIT and Apache licenses are included alongside the source.

`zed-async-tar` in GPUI 0.2.2 depends on this release. Its Linux/Android
implementation refers to `libc::ENOATTR`, a deprecated alias removed in libc
0.2.190. Use `libc::ENODATA` on Linux and Android, preserving the existing
behavior and the API expected by the dependency. Other platforms retain their
original constants. This avoids downgrading libc for the whole application.
