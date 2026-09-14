import assert from "node:assert/strict";
import test from "node:test";
import { readFile, readdir } from "node:fs/promises";

import { ICONS } from "./icons.mjs";

const TAGS = new Set(["path", "rect", "circle", "line", "polyline", "polygon", "ellipse"]);
const SVG_NS = "http://www.w3.org/2000/svg";
// The two arrangement modules draw display diagrams, not icons.
const DRAWS_DIAGRAMS = new Set(["arrangement-render.mjs", "icons.mjs"]);

async function appSources() {
  const dir = new URL("./", import.meta.url);
  const names = (await readdir(dir)).filter(
    (file) =>
      /\.(mjs|js|html)$/.test(file) &&
      !file.endsWith(".test.mjs") &&
      file !== "harness.html" &&
      !file.startsWith("trial"),
  );
  return Promise.all(names.map(async (file) => [file, await readFile(new URL(file, dir), "utf8")]));
}

test("every vendored icon is Lucide element data with plain attributes", () => {
  for (const [name, nodes] of Object.entries(ICONS)) {
    assert.match(name, /^[a-z0-9]+(-[a-z0-9]+)*$/, name);
    assert.ok(Array.isArray(nodes) && nodes.length > 0, name);
    for (const [tag, attrs] of nodes) {
      assert.ok(TAGS.has(tag), `${name}: ${tag}`);
      for (const [key, value] of Object.entries(attrs)) {
        assert.match(key, /^[a-z][a-z0-9]*$/, `${name}: ${key}`);
        assert.equal(typeof value, "string", `${name}: ${key}`);
      }
    }
  }
});

test("every icon the app names is vendored, and every vendored icon is used", async () => {
  const sources = await appSources();
  const named = new Set();
  for (const [, source] of sources) {
    for (const match of source.matchAll(/iconName(?: ?[:=]|\s\?\?) "([a-z0-9-]+)"/g))
      named.add(match[1]);
    for (const match of source.matchAll(/\bicon\("([a-z0-9-]+)"/g)) named.add(match[1]);
    for (const match of source.matchAll(/lucide-([a-z0-9-]+)/g)) named.add(match[1]);
    for (const match of source.matchAll(/GLYPHS = \{([^}]*)\}/g))
      for (const value of match[1].matchAll(/"([a-z0-9-]+)"/g)) named.add(value[1]);
  }
  for (const name of named) assert.ok(name in ICONS, `missing icon: ${name}`);
  for (const name of Object.keys(ICONS))
    assert.ok(
      sources.some(
        ([, source]) => source.includes(`"${name}"`) || source.includes(`lucide-${name}`),
      ),
      `unused icon: ${name}`,
    );
});

test("no screen draws its own icon; static markup in index.html is Lucide", async () => {
  for (const [file, source] of await appSources()) {
    if (DRAWS_DIAGRAMS.has(file)) continue;
    assert.doesNotMatch(source, new RegExp(SVG_NS.replace(/[/.]/g, "\\$&")), file);
    if (file.endsWith(".html")) {
      for (const match of source.matchAll(/<svg\b[^>]*>/g))
        assert.match(match[0], /class="lucide lucide-[a-z0-9-]+/, `${file}: ${match[0]}`);
    }
  }
});
