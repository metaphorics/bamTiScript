import { toASCII } from "../projects/ufo/src/punycode.ts";

// Exercise ufo's vendored punycode toASCII encoder on project-derived inputs.
// toASCII converts Unicode hostnames to Punycode ACE form (RFC 3490).
const inputs = [
  "münchen.de",
  "日本語.jp",
  "café.fr",
  "naïve.org",
  "example.com",
];

for (const host of inputs) {
  console.log(`${host} -> ${toASCII(host)}`);
}
