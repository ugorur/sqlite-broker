# Versions

Releases are Git tags of the form `vMAJOR.MINOR.PATCH`. The tag `v0.1.0` must match `version` in the root `Cargo.toml`:

```toml
[workspace.package]
version = "0.1.0"
```

Pushing the tag builds `ghcr.io/ugorur/sqlite-broker` and publishes these tags:

| Git tag | Image tags |
| --- | --- |
| `v0.1.0` | `0.1.0`, `0.1`, `0`, `latest` |
| `v0.1.1` | `0.1.1`, `0.1`, `0`, `latest` |
| `v0.2.0` | `0.2.0`, `0.2`, `0`, `latest` |
| `v1.0.0` | `1.0.0`, `1.0`, `1`, `latest` |
| `v0.2.0-rc.1` | `0.2.0-rc.1` only |

A pre-release tag contains `-` and does not move `latest`.

`sqlite-broker version` prints the version compiled into the binary.

## Cutting a release

1. Set `version` in `Cargo.toml` to the release, for example `0.1.0`.
2. Update the image tags in `examples/` if you want the examples to follow that release.
3. Commit, push `main`, then:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The [image workflow](../.github/workflows/image.yml) refuses the tag when it does not match `Cargo.toml`.
