import type { JsonValue, RequestOptions } from "../index.js";

export {
  BamtiError,
  DisposedError,
  ProtocolError,
  RestartLimitError,
  ServiceRequestError,
  Transport,
  TransportBusyError,
  TransportCrashError,
} from "../index.js";

export interface TextRange {
  readonly start: number;
  readonly end: number;
}

export interface DocumentSnapshot {
  readonly path: string;
  readonly version: number;
  readonly open: boolean;
}

export interface ProjectSnapshot {
  readonly documents: readonly DocumentSnapshot[];
}

export type SymbolKind =
  | "intrinsicValue"
  | "intrinsicType"
  | "var"
  | "let"
  | "const"
  | "using"
  | "awaitUsing"
  | "function"
  | "parameter"
  | "class"
  | "interface"
  | "typeAlias"
  | "enum"
  | "enumMember"
  | "typeParameter"
  | "import"
  | "namespace";

export type QuickInfoKind = SymbolKind | "property";

export interface Completion {
  readonly name: string;
  readonly kind: SymbolKind;
  readonly replacement: TextRange;
}

export interface Location {
  readonly path: string;
  readonly range: TextRange;
}

export interface DocumentEdit extends Location {
  readonly replacement: string;
}

export interface RenameResult {
  readonly symbol: string;
  readonly edits: readonly DocumentEdit[];
}

export interface QuickInfo {
  readonly name: string;
  readonly kind: QuickInfoKind;
  readonly typeDisplay: string;
  readonly display: string;
  readonly range: TextRange;
}

export interface Diagnostic {
  readonly path: string;
  readonly range: TextRange;
  readonly code: string;
  readonly severity: "error" | "warning";
  readonly message: string;
}

export interface OpenParams {
  readonly path: string;
  readonly text: string;
  readonly version: number;
}

export type UpdateParams = OpenParams;

export interface PathParams {
  readonly path: string;
}

export interface PositionParams extends PathParams {
  readonly position: number;
}

export interface RenameParams extends PositionParams {
  readonly newName: string;
}

export interface CloseResult {
  readonly path: string;
  readonly closed: true;
}

export interface ServiceApi {
  open(params: OpenParams, options?: RequestOptions): Promise<DocumentSnapshot>;
  update(params: UpdateParams, options?: RequestOptions): Promise<DocumentSnapshot>;
  close(params: PathParams, options?: RequestOptions): Promise<CloseResult>;
  snapshot(params?: Readonly<Record<string, never>>, options?: RequestOptions): Promise<ProjectSnapshot>;
  completions(params: PositionParams, options?: RequestOptions): Promise<readonly Completion[]>;
  definition(params: PositionParams, options?: RequestOptions): Promise<Location | null>;
  quickInfo(params: PositionParams, options?: RequestOptions): Promise<QuickInfo | null>;
  references(params: PositionParams, options?: RequestOptions): Promise<readonly Location[]>;
  rename(params: RenameParams, options?: RequestOptions): Promise<RenameResult>;
  diagnostics(params: PathParams, options?: RequestOptions): Promise<readonly Diagnostic[]>;
}

export declare const MAX_HEADER_BYTES: number;
export declare const MAX_FRAME_BYTES: number;

export function encodeFrame(
  message: JsonValue,
  options?: { maxFrameBytes?: number },
): Uint8Array;

export class FrameDecoder {
  constructor(options?: { maxHeaderBytes?: number; maxFrameBytes?: number });
  readonly maxHeaderBytes: number;
  readonly maxFrameBytes: number;
  readonly bufferedBytes: number;
  push(chunk: Uint8Array): JsonValue[];
  end(): void;
}
