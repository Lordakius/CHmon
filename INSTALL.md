# Install

Please note I can only support Linux builds, so everything else should be taken with a grain of salt.
Furthermore right now I recommend building from source.

If you want to maintain other builds (or just test if they work), feel free to open a PR containing 
instructions.

## Manual Installation

1. [Prerequisites](#prerequisites)
   1. [Source Code](#clone-the-source-code)
2. [Building](#building)
   1. [Linux/Windows](#linux--windows)
   2. [macOS](#macos)
   3. [Compatibility build](#compatibility-build)

### Prerequisites

#### Clone the source code

Before compiling, you need the source code

```sh
git clone https://github.com/Lordakius/CHmon.git
cd CHmon 
```

### Building

#### Linux / Windows

```sh
cargo build --release
```

The application executable will be built to

```sh
target/release/chmon
```

#### Compatibility build

CHmon is built using `wgpu` which has [requirements](https://github.com/gfx-rs/wgpu#supported-platforms)
which might not be achievable by all.
It is therefore possible to build a compatability build using `opengl`
as renderer instead. Performance should be close to 1:1.

To build a compatability build add the flag `--no-default-features --features opengl`

```sh
cargo build --release --no-default-features --features opengl
```
