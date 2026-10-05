# RustPBX Addon Architecture

RustPBX is built from a small core plus optional addons. An addon is a Rust crate
that implements the `Addon` trait; it is compiled in through a Cargo feature and
assembled at startup by the application binary.

Community addons live in this repository. Commercial addons ship as separate
repositories and are not part of this tree.

## 1. Components

1. **Addon trait** — lifecycle hooks, API/UI routes and sidebar injection
   (`src/addons/mod.rs`).
2. **Addon registry** — collects the enabled addons and merges their routers
   (`src/addons/registry.rs`).
3. **Feature flags** — control which addons are compiled (`Cargo.toml`).
4. **Application binary** — composes the addons through `AppBuilder`.

## 2. Directory structure

```
src/addons/
  mod.rs          # Addon trait and shared types
  registry.rs     # Addon registry
  events.rs       # Addon-facing event channels
  acme/           # ACME certificate management
  archive/        # Call archive
  transcript/     # Transcription
  observability/  # Prometheus metrics
  queue/          # Queue engine
```

## 3. The Addon trait

```rust
#[async_trait]
pub trait Addon: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str { "" }
    fn category(&self) -> AddonCategory { AddonCategory::Community }

    async fn initialize(&self, state: AppState) -> anyhow::Result<()>;
    fn router(&self, state: AppState) -> Option<Router>;
    fn sidebar_items(&self) -> Vec<SidebarItem> { vec![] }
    fn migrations(&self) -> Vec<Box<dyn MigrationTrait>> { vec![] }
}
```

## 4. Feature flags

```toml
[features]
addon-acme = ["console", "instant-acme"]
addon-archive = ["console", "dep:async-compression", "dep:csv-async"]
addon-transcript = ["console"]
addon-observability = ["metrics-exporter-prometheus"]

default = [
    "console",
    "addon-acme",
    "addon-transcript",
    "addon-archive",
    "addon-observability",
]
```

## 5. Composing an application

The community binary is a thin wrapper around `AppBuilder`. A distribution that
adds its own addons passes them to `main_with_addons`:

```rust
fn main() -> anyhow::Result<()> {
    rustpbx::builder::main_with_addons(vec![
        std::sync::Arc::new(MyAddon::new()),
    ])
}
```

`src/bin/rustpbx.rs` is the reference composition. `examples/external-demo-addon`
shows a complete addon implemented without touching core sources.

## 6. Registering an addon in the core build

```rust
#[cfg(feature = "addon-acme")]
pub mod acme;

#[cfg(feature = "addon-archive")]
pub mod archive;
```

Each addon is listed in the matching `Cargo.toml` feature and registered by the
binary that needs it. Per-addon database migrations are tracked in a dedicated
table so an addon can be added or removed without touching the others.

## 7. Commercial addons

Commercial addons follow the same `Addon` trait but live in their own
repositories (for example `rustpbx-commerce`, `rustpbx-cc`,
`rustpbx-wholesale`) and are pulled in as dependencies by those distributions.
The community repository contains no commercial addon sources.
