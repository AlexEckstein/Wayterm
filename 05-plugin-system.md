# Plugin System

## Overview

Terminal backends are pluggable. The primary mechanism is Rust's trait
system + Cargo feature flags (compile-time selection). An optional C ABI
plugin system allows runtime-loading of backends as dynamic libraries.

## Two Plugin Mechanisms

### 1. Compile-Time: Cargo Features (Primary)

Built-in backends are selected via Cargo features. This is the simplest
and most Rust-idiomatic approach:

```toml
# Cargo.toml
[features]
default = ["backend-vte"]
backend-vte = ["vte4"]              # Always available, GTK4 is required anyway
backend-ghostty = ["libloading", "libghostty-sys"]  # Future
backend-alacritty = ["alacritty_terminal"]  # Possible intermediate
```

```rust
// src/backend/mod.rs
pub fn create_backend(
    name: &str,
    event_tx: mpsc::Sender<TerminalEvent>,
) -> Box<dyn TerminalBackend> {
    match name {
        #[cfg(feature = "backend-vte")]
        "vte" => Box::new(VteBackend::new(event_tx)),
        #[cfg(feature = "backend-ghostty")]
        "ghostty" => Box::new(GhosttyBackend::new(event_tx)),
        #[cfg(feature = "backend-alacritty")]
        "alacritty" => Box::new(AlacrittyBackend::new(event_tx)),
        _ => {
            #[cfg(feature = "backend-vte")]
            { Box::new(VteBackend::new(event_tx)) }
            #[cfg(not(feature = "backend-vte"))]
            { panic!("No terminal backend compiled in"); }
        }
    }
}

pub fn available_backends() -> Vec<&'static str> {
    let mut backends = Vec::new();
    #[cfg(feature = "backend-vte")]
    backends.push("vte");
    #[cfg(feature = "backend-ghostty")]
    backends.push("ghostty");
    #[cfg(feature = "backend-alacritty")]
    backends.push("alacritty");
    backends
}
```

**Advantages:**
- Zero runtime overhead — direct dispatch, no FFI
- Compiler-enforced type safety across the trait boundary
- No dlopen, no missing-dependency crashes
- Standard Cargo workflow — users build with `--features backend-ghostty`

**Disadvantages:**
- Backend must be chosen at compile time
- Users can't add a new backend without recompiling
- No third-party plugins without upstreaming the feature

### 2. Runtime: C ABI Plugins (Optional, Future)

For cases where runtime backend loading is desired (third-party plugins,
user-installed backends without recompilation), a C ABI system is used.

This wraps the Rust `TerminalBackend` trait in C-compatible functions
and loads them via `libloading`:

```rust
use libloading::{Library, Symbol};

// C ABI wrapper types
#[repr(C)]
pub struct CTerminalFeatures {
    pub search: bool,
    pub color_schemes: bool,
    pub scrollback_search: bool,
    pub url_detection: bool,
    pub sixel_graphics: bool,
    pub kitty_graphics: bool,
    pub custom_fonts: bool,
    pub reflow: bool,
    pub alternate_screen: bool,
}

// Plugin .so exports these C functions:
//   const char* terminal_plugin_name();
//   const char* terminal_plugin_version();
//   const char* terminal_plugin_description();
//   CTerminalFeatures terminal_plugin_features();
//   void* terminal_plugin_create(EventHandlerFn);
//   void terminal_plugin_destroy(void* handle);
//   void terminal_plugin_write(void* handle, const uint8_t* data, size_t len);
//   void terminal_plugin_set_size(void* handle, uint32_t rows, uint32_t cols);
//   ...
//   bool terminal_plugin_available();  // dependency check

type CreateFn = unsafe extern "C" fn(EventHandlerFn) -> *mut c_void;
type DestroyFn = unsafe extern "C" fn(*mut c_void);
type WriteFn = unsafe extern "C" fn(*mut c_void, *const u8, usize);
type FeaturesFn = unsafe extern "C" fn() -> CTerminalFeatures;
type AvailableFn = unsafe extern "C" fn() -> bool;

pub struct LoadedPlugin {
    _lib: Library,  // kept alive — dropping closes the .so
    name: String,
    create: CreateFn,
    destroy: DestroyFn,
    write: WriteFn,
    features: FeaturesFn,
    available: AvailableFn,
}

pub struct PluginRegistry {
    built_in: Vec<&'static str>,
    loaded: Vec<LoadedPlugin>,
}

impl PluginRegistry {
    pub fn discover(plugin_dir: &Path) -> Self {
        let mut registry = PluginRegistry {
            built_in: available_backends(),
            loaded: Vec::new(),
        };

        // Scan plugin directory for .so files
        if let Ok(entries) = fs::read_dir(plugin_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension() == Some(OsStr::new("so")) {
                    if let Ok(plugin) = load_plugin(&path) {
                        if (plugin.available)() {
                            registry.loaded.push(plugin);
                        }
                    }
                }
            }
        }

        registry
    }

    pub fn all_backends(&self) -> Vec<BackendInfo> {
        let mut info: Vec<BackendInfo> = self.built_in.iter()
            .map(|name| BackendInfo {
                name: name.to_string(),
                built_in: true,
                available: true,
            })
            .collect();

        for plugin in &self.loaded {
            info.push(BackendInfo {
                name: plugin.name.clone(),
                built_in: false,
                available: true,
            });
        }

        info
    }
}

fn load_plugin(path: &Path) -> Result<LoadedPlugin, Box<dyn std::error::Error>> {
    unsafe {
        let lib = Library::new(path)?;

        let name_fn: Symbol<extern "C" fn() -> *const c_char> = lib.get(b"terminal_plugin_name")?;
        let create_fn: Symbol<CreateFn> = lib.get(b"terminal_plugin_create")?;
        let destroy_fn: Symbol<DestroyFn> = lib.get(b"terminal_plugin_destroy")?;
        let write_fn: Symbol<WriteFn> = lib.get(b"terminal_plugin_write")?;
        let features_fn: Symbol<FeaturesFn> = lib.get(b"terminal_plugin_features")?;
        let available_fn: Symbol<AvailableFn> = lib.get(b"terminal_plugin_available")?;

        let name = CStr::from_ptr(name_fn()).to_string_lossy().into_owned();

        Ok(LoadedPlugin {
            _lib: lib,
            name,
            create: *create_fn,
            destroy: *destroy_fn,
            write: *write_fn,
            features: *features_fn,
            available: *available_fn,
        })
    }
}
```

### C ABI Plugin Implementation (Rust side, for plugin authors)

A plugin author writing a backend in Rust would export the C ABI:

```rust
// In the plugin crate (e.g., ghostty-backend-plugin)
use std::os::raw::{c_char, c_void};

static PLUGIN_NAME: &[u8] = b"ghostty-vt\0";
static PLUGIN_VERSION: &[u8] = b"0.1.0\0";
static PLUGIN_DESC: &[u8] = b"libghostty-vt terminal engine\0";

#[no_mangle]
pub extern "C" fn terminal_plugin_name() -> *const c_char {
    PLUGIN_NAME.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn terminal_plugin_version() -> *const c_char {
    PLUGIN_VERSION.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn terminal_plugin_description() -> *const c_char {
    PLUGIN_DESC.as_ptr() as *const c_char
}

#[no_mangle]
pub extern "C" fn terminal_plugin_available() -> bool {
    // Check if libghostty-vt is loadable
    libloading::Library::new("libghostty_vt.so").is_ok()
}

#[no_mangle]
pub extern "C" fn terminal_plugin_create() -> *mut c_void {
    let backend = Box::new(GhosttyBackend::new());
    Box::into_raw(backend) as *mut c_void
}

#[no_mangle]
pub extern "C" fn terminal_plugin_destroy(handle: *mut c_void) {
    unsafe {
        drop(Box::from_raw(handle as *mut GhosttyBackend));
    }
}

#[no_mangle]
pub extern "C" fn terminal_plugin_write(handle: *mut c_void, data: *const u8, len: usize) {
    let backend = unsafe { &*(handle as *mut GhosttyBackend) };
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    backend.write(slice);
}
```

## Backend Selection

At startup:

1. `PluginRegistry::discover()` scans for built-in + dynamic plugins
2. Filters to available backends
3. User preference from settings (or "auto" = best available)
4. Settings UI presents available backends

```rust
// Settings UI
let store = gtk4::ListStore::new(&[glib::Type::STRING, glib::Type::STRING]);
for backend in registry.all_backends() {
    let label = if backend.built_in {
        format!("{} (built-in)", backend.name)
    } else {
        format!("{} (plugin)", backend.name)
    };
    store.set(&store.append(), &[0, 1], &[&label, &backend.name]);
}
```

## Per-Session Backend Override

```rust
pub struct SessionConfig {
    // ... connection details ...
    pub preferred_backend: Option<String>,  // None = use global default
}
```

## Security Considerations

- Built-in backends (Cargo features): no security concern — compiled in,
  type-safe, no dlopen
- C ABI plugins: loaded from a fixed system directory
  (`/usr/lib/yourapp/plugins/` or `~/.local/share/yourapp/plugins/`),
  not arbitrary paths
- Plugins receive raw terminal data, not connection credentials
- `libloading::Library` keeps the .so resident — don't drop while
  backend instances exist
- C ABI plugin authors must handle thread safety — the `write()` function
  may be called from the GTK main thread, while the plugin's internal
  state must be `Send`-safe

## Packaging

### Debian/Ubuntu
```
yourapp                 — main app, VTE backend built-in (default features)
yourapp-backend-ghostty — libghostty-vt plugin (separate .so + deps)
```

### Flatpak / AppImage
Bundle all compiled-in features. VTE is always available (GTK4 is required
anyway). Ghostty backend is a compile-time feature.

### Cargo (developer builds)
```bash
# Default (VTE only)
cargo build --release

# With ghostty backend (when available)
cargo build --release --features backend-ghostty

# With all backends
cargo build --release --all-features
```

## Design Decision: Why Both Mechanisms?

**Cargo features** handle the 95% case: built-in backends, known at
compile time, type-safe. This is what most users and contributors need.

**C ABI plugins** handle the 5% case: a third party wants to write a
backend in another language (C, Zig, Go with cgo), or distribute a
backend as a separate package without recompiling the main app.

Starting with Cargo features only (VTE built-in). Add the C ABI layer
when there's actual demand for third-party plugins. The trait abstraction
supports both — no architecture change needed to add C ABI later.