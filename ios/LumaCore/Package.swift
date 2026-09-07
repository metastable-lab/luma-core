// swift-tools-version:5.9
import PackageDescription

// A LOCAL package that carries the generated UniFFI Swift bindings, so the demo app
// (and anything else) reaches the Rust protocol with one `import LumaCore`.
//
// Nothing here is committed except this manifest and `shim.c`: `ios/build-core.sh`
// generates the Swift, the header and the module map, and builds the two static
// archives into `ios/lib/`. Run it before opening the Xcode project.
//
// No linker settings, deliberately. SPM rejects `unsafeFlags` in a consumed package,
// and none are needed here: compiling never links. The APP project supplies
// `-lluma_core` plus the per-SDK library search paths at final link — and those
// must sit at PROJECT level, not target level, or the app links clean while any test
// bundle fails on undefined `uniffi_luma_core_*` symbols.
let package = Package(
    name: "LumaCore",
    platforms: [.iOS(.v17)],
    products: [
        .library(name: "LumaCore", targets: ["LumaCore"]),
    ],
    targets: [
        // The C module holding the Rust FFI header and its module map, both GENERATED.
        // The name is fixed by UniFFI, not chosen here: the generated Swift opens with
        // `#if canImport(LumaCoreFFI)`.
        //
        // `shim.c` is committed because an SPM C target with no sources fails to
        // resolve — without it a fresh clone could not even open this package.
        .target(
            name: "LumaCoreFFI",
            publicHeadersPath: "include"
        ),
        .target(
            name: "LumaCore",
            dependencies: ["LumaCoreFFI"]
        ),
    ]
)
