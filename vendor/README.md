# Vendored upstream sources

`waypipe/` is an unmodified checkout of
`https://gitlab.freedesktop.org/mstoeckl/waypipe.git` at tag `v0.11.3`, commit
`3ea049b2d33cbf1000714c28b9668f8c244f70bd`.

It is excluded from the Cargo workspace. The upstream mainloop requires Linux
facilities such as `memfd`, `pipe2`, and `ppoll`, so it is used as the protocol
reference and integration-test peer rather than linked into non-Linux clients.
