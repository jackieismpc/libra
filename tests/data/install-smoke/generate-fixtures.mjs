// Regenerate the install-smoke fixtures (plan-20260821 A1-05).
//
//   node tests/data/install-smoke/generate-fixtures.mjs
//
// Fixtures are signed with the SAME committed test-only Ed25519 keypair as
// the UP-01 contract vectors (tests/data/up01-transition-vectors-v1.json).
// The private half is intentionally public: it protects nothing, it exists
// so the smoke harness can exercise the real verification code paths.
// Deterministic: fixed timestamps, fixed contents, RFC 8032 deterministic
// signatures — rerunning must be byte-stable.

import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { webcrypto as crypto, createHash } from "node:crypto";

const TEST_PRIVATE_KEY_PKCS8_B64 =
  "MC4CAQAwBQYDK2VwBCIEIPHTtu4stYxMs50qKjn1e04Hei8LuAiJ5LorBvbK0ici";
const KEY_ID = "libra-release-test-1";
const DOMAIN = "libra-upgrade-manifest-v1\0";
const PLATFORMS = ["linux-amd64", "linux-arm64", "darwin-arm64", "windows-amd64"];
const VERSION = "9.9.9";

const root = dirname(fileURLToPath(import.meta.url));

function sha256Hex(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

async function sign(payloadBytes) {
  const key = await crypto.subtle.importKey(
    "pkcs8",
    Buffer.from(TEST_PRIVATE_KEY_PKCS8_B64, "base64"),
    { name: "Ed25519" },
    false,
    ["sign"],
  );
  const message = Buffer.concat([Buffer.from(DOMAIN, "binary"), payloadBytes]);
  return Buffer.from(await crypto.subtle.sign("Ed25519", key, message));
}

async function envelope(payload) {
  const payloadBytes = Buffer.from(JSON.stringify(payload));
  const signature = await sign(payloadBytes);
  return `${JSON.stringify({
    schema_version: 1,
    payload: payloadBytes.toString("base64"),
    signatures: [{ key_id: KEY_ID, signature: signature.toString("base64") }],
  })}\n`;
}

function payloadFor(binaryByPlatform, mutate = {}) {
  return {
    channel: "stable",
    version: VERSION,
    control_revision: 3,
    published_at: "2026-01-01T00:00:00.000Z",
    expires_at: "2028-01-01T00:00:00.000Z",
    min_key_generation: 1,
    paused: false,
    revoked_versions: [],
    artifacts: PLATFORMS.map((platform) => ({
      platform,
      url: `https://download.libra.tools/libra/releases/v${VERSION}/libra-${platform}`,
      sha256: mutate.sha256 ?? sha256Hex(binaryByPlatform[platform]),
      size: mutate.size ?? binaryByPlatform[platform].length,
    })),
  };
}

function writeFixture(path, content) {
  mkdirSync(dirname(join(root, path)), { recursive: true });
  writeFileSync(join(root, path), content);
  console.log(`wrote ${path}`);
}

const binaries = Object.fromEntries(
  PLATFORMS.map((platform) => [
    platform,
    Buffer.from(`libra install-smoke fixture binary ${VERSION} ${platform}\n`),
  ]),
);

// Artifact tree shared by the signed scenarios.
for (const platform of PLATFORMS) {
  writeFixture(
    `fixtures/tree/libra/releases/v${VERSION}/libra-${platform}`,
    binaries[platform],
  );
}

// Legacy-fallback tree (v9.9.8): binary + .sha256 sidecar, no manifest.
// The Windows installer's legacy path downloads the `.exe` asset name.
const legacy = Buffer.from("libra install-smoke legacy binary 9.9.8\n");
for (const platform of PLATFORMS) {
  writeFixture(`fixtures/tree/libra/releases/v9.9.8/libra-${platform}`, legacy);
  writeFixture(
    `fixtures/tree/libra/releases/v9.9.8/libra-${platform}.sha256`,
    `${sha256Hex(legacy)}  libra-${platform}\n`,
  );
}
writeFixture(
  "fixtures/tree/libra/releases/v9.9.8/libra-windows-amd64.exe",
  legacy,
);
// Mirrors the GitHub API's pretty-printed shape: install.sh's legacy-path
// extraction expects `"tag_name": "..."` with a space after the colon.
writeFixture(
  "fixtures/tree/repos/libra-tools/libra/releases/latest",
  `${JSON.stringify({ tag_name: "v9.9.8" }, null, 2)}\n`,
);

// Scenario manifests.
writeFixture("fixtures/manifest-valid.json", await envelope(payloadFor(binaries)));

const badSignature = JSON.parse(await envelope(payloadFor(binaries)));
const sigBytes = Buffer.from(badSignature.signatures[0].signature, "base64");
sigBytes[0] ^= 0xff;
badSignature.signatures[0].signature = sigBytes.toString("base64");
writeFixture(
  "fixtures/manifest-bad-signature.json",
  `${JSON.stringify(badSignature)}\n`,
);

writeFixture(
  "fixtures/manifest-sha-mismatch.json",
  await envelope(payloadFor(binaries, { sha256: "f".repeat(64) })),
);
writeFixture(
  "fixtures/manifest-size-mismatch.json",
  await envelope(payloadFor(binaries, { size: 1 })),
);
