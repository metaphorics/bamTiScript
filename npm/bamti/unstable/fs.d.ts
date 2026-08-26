export interface OsFileSystem {
  readonly kind: "os";
  readonly root: string;
}

export function osFileSystem(root?: string): OsFileSystem;
