## Where things live
- `lib.rs` + modules: raw schema (`raw.rs`), validation, catalogs, plane narrowing (`ValidatedConfig::for_plane`: `Private` drops the public listener; `Public` keeps only public routes, their adapters and keys without owner/private-provider scopes). `reload.rs` lists restart-only settings; `commands.rs` holds `kanata config compact|expand|plan`.
- `keys/`: the `keys.toml` schema (`file.rs`), timestamps (`time.rs`) and `DailyQuota`, shared with `kanata-keys`; `secret.rs`: secret resolution and bearer-token canonicalisation used by auth and adapters.

## Config conventions
- `Raw*` structs are `#[serde(deny_unknown_fields)]`; `validate()` produces `Validated*` types with private fields and getters.
- Errors: `ConfigError::new(path, ConfigErrorClass::Variant)`; classes live in `error.rs` and every one is listed in the `config/README.md` validation-error table (a test enforces it). Never put secret values in paths.
- Optional settings: `#[serde(default)] Option<_>`, defaults applied in validation.
