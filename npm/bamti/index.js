import { run as runCli } from "bamti-cli";

export {
  ArtifactNotFoundError,
  UnsupportedPlatformError,
  artifactPackage,
  resolveBinary,
  resolveBinary as resolveArtifact,
} from "bamti-cli";
export {
  BamtiError,
  DisposedError,
  ProtocolError,
  RestartLimitError,
  ServiceRequestError,
  TransportBusyError,
  TransportCrashError,
} from "./internal/errors.js";
export { Session, createSession } from "./internal/session.js";
export { Transport } from "./internal/transport.js";

export function run(args = [], options = {}) {
  return runCli(args, options);
}
