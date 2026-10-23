import { defineConfig } from "@hey-api/openapi-ts";

// The backend gRPC proto (`contracts/proto`) is the source of truth. `protoc` with
// `protoc-gen-connect-openapi` emits `contracts/openapi.json` from it; this regenerates
// the TypeScript types from that artifact (`npm run gen:api`, wired as the flake
// `gen-api` app + pre-commit hook). The cabinet backend (Rust) reaches the hub over
// gRPC, so these are TYPES ONLY — snake_case property names matching the wire shape the
// backend emits — not an HTTP client. Output is committed so the app type-checks
// without the backend.
export default defineConfig({
  input: "../../contracts/openapi.json",
  output: { path: "shared/contracts/gen" },
  plugins: ["@hey-api/typescript"],
});
