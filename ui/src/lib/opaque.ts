/**
 * OPAQUE client wrapper.
 *
 * On web we use `@serenity-kit/opaque`, which loads a WebAssembly module.
 * The library exposes a `ready` promise that must resolve before any
 * cryptographic operations are used.
 */
import * as opaque from "@serenity-kit/opaque";

export { opaque };

/**
 * Ensure the OPAQUE WASM module is loaded.
 */
export async function ensureOpaqueReady(): Promise<void> {
	await opaque.ready;
}
