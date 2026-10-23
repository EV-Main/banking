// Server-side reader for the microfrontend registry.
//
// The registry maps logical names to {tag, scriptUrl, kind}. The cabinet server reads
// `mfe-registry.json` from its working directory, so an independent deploy lands by
// editing that file — never by rebuilding cabinet.

import { promises as fs } from "node:fs";
import path from "node:path";

import type { MfeEntry } from "./types";
import { parseRegistry } from "./validate";

export async function loadRegistry(): Promise<MfeEntry[]> {
  const file = path.join(process.cwd(), "mfe-registry.json");
  const raw = await fs.readFile(file, "utf8");
  return parseRegistry(JSON.parse(raw));
}

export async function findMfe(name: string): Promise<MfeEntry | undefined> {
  const registry = await loadRegistry();
  return registry.find((entry) => entry.name === name);
}
