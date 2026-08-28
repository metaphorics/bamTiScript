import { run as runCli } from "bamti-cli";
import { loadNativeAddon } from "./native-loader.js";

export {
  NATIVE_TARGETS,
  NativeArtifactLoadError,
  NativeArtifactNotFoundError,
  UnsupportedPlatformError,
  loadNativeAddon,
  selectNativeTarget,
} from "./native-loader.js";
export {
  ArtifactNotFoundError,
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

export async function runNative(request) {
  return loadNativeAddon().run(request);
}
