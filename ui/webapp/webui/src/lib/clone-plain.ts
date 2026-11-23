/**
 * Deep-clone a JSON-able value, including Vue reactive / readonly proxies.
 *
 * `structuredClone` uses the HTML structured-clone algorithm and throws
 * `DataCloneError` on a `Proxy` — which is exactly what Vue 3 (and Pinia
 * `ref` / `reactive` state) wraps around kernel JSON after the settings
 * stores landed. `JSON.stringify` walks the same enumerables through the
 * proxy getters and yields a plain object, which is also the on-the-wire
 * shape of execution-policy and blueprint payloads.
 *
 * Use this anywhere the panel would otherwise `structuredClone` a value
 * that may have come from a store or a `v-model` buffer.
 */
export function clonePlain<T>(value: T): T {
  if (value === undefined) return value
  return JSON.parse(JSON.stringify(value)) as T
}
