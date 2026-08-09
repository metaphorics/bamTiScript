use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

const EXPECTED_STDOUT: &[u8] = b"hello from bamts\n";
const UTF16_PROGRAM: &str = r#"const s = "\uD800";
process.stdout.write(String(s.length) + "\n");
process.stdout.write(s.charCodeAt(0).toString(16) + "\n");
process.stdout.write(s.codePointAt(0).toString(16) + "\n");
process.stdout.write(String(/./gu.exec("\u{1F600}")[0].length) + "\n");
process.stdout.write(String("\u{10003}" < "\u{E000}") + "\n");
const key = Object.keys({["\u{1F600}"]: 3})[0];
process.stdout.write(String(key.length) + "\n");
process.stdout.write(key.codePointAt(0).toString(16) + "\n");
"#;
const UTF16_STDOUT: &[u8] = b"1\nd800\nd800\n2\ntrue\n2\n1f600\n";
const CALLABLE_PROGRAM: &str = r#"function probe(a: unknown, b: unknown) {
    const values = [this, a, b];
    return values;
}

const receiver = { tag: "right" };
const ignored = { tag: "wrong" };
const applied = probe.apply(receiver, { 0: 7, 1: 8, length: 2 });
if (applied[0] !== receiver || applied[1] !== 7 || applied[2] !== 8) {
    throw "apply mismatch";
}

const bound = probe.bind(receiver, 1);
const called = bound.call(ignored, 2);
if (called[0] !== receiver || called[1] !== 1 || called[2] !== 2) {
    throw "bind mismatch";
}
if (bound.length !== 1 || bound.name !== "bound probe") {
    throw "bound metadata mismatch";
}
if (Object.hasOwn(bound, "prototype")) {
    throw "bound shape mismatch";
}

function Box(a: number, b: number) {
    this.sum = a + b;
}
Object.defineProperty(Box, "prototype", {
    value: { kind: "box" },
    writable: true,
});
const BoundBox = Box.bind({ sum: 99 }, 4);
const box = new BoundBox(5);
if (
    box.sum !== 9 ||
    box.kind !== "box" ||
    !(box instanceof Box) ||
    !(box instanceof BoundBox)
) {
    throw "bound construction mismatch";
}
"#;
const VM_PROGRAM: &str = r#"import { runInNewContext } from 'node:vm';
console.log(runInNewContext('1 + 1'));
console.log(typeof runInNewContext('({})'));
"#;
const CLASSIC_DYNAMIC_IMPORT_PROGRAM: &str = r#"import vm from 'node:vm';
vm.runInThisContext("import('node:util').then(function(ns) { process.stdout.write(String(typeof ns.parseArgs) + '\\n'); })");
"#;
static NEXT_DIRECTORY: AtomicU32 = AtomicU32::new(0);

#[test]
fn run_fixture_preserves_stdout_and_exit_code() {
    let directory = ScratchDirectory::new();
    let output = directory
        .command()
        .args(["run", "--target", "jit"])
        .arg(fixture())
        .output()
        .expect("bamts run starts");
    assert_success(&output, "bamts run");
    assert_eq!(output.stdout, EXPECTED_STDOUT);
}

#[test]
fn aot_and_jit_share_process_argv_entrypoint_parity() {
    let project = ScratchDirectory::new();
    project.write(
        "main.ts",
        r#"process.stdout.write(process.argv[0] + "\n");
process.stdout.write(process.argv[1] + "\n");
process.stdout.write(process.argv[2] + "\n");
process.stdout.write(process.argv[3] + "\n");
process.stdout.write(process.env.BAMTS_AOT_ENTRYPOINT === undefined ? "hidden\n" : "leaked\n");
"#,
    );

    let jit = project
        .command()
        .env_remove("BAMTS_AOT_ENTRYPOINT")
        .args(["run", "--target", "jit", "main.ts", "--", "first", "second"])
        .current_dir(&project.path)
        .output()
        .expect("bamts JIT argv program starts");
    assert_success(&jit, "bamts JIT argv program");

    let aot = project
        .command()
        .env_remove("BAMTS_AOT_ENTRYPOINT")
        .args(["run", "--target", "aot", "main.ts", "--", "first", "second"])
        .current_dir(&project.path)
        .output()
        .expect("bamts AOT argv program starts");
    assert_success(&aot, "bamts AOT argv program");

    assert_eq!(jit.stdout, b"bamts\nmain.ts\nfirst\nsecond\nhidden\n");
    assert_eq!(aot.stdout, jit.stdout);
}

#[test]
fn aot_and_jit_execute_non_decimal_bigint_literals() {
    let project = ScratchDirectory::new();
    project.write(
        "main.ts",
        r#"const hex = 0x100000000000000000000000000000001n;
const octal = 0o20n;
const binary = 0b1_0000n;
if (
    hex !== 340282366920938463463374607431768211457n ||
    octal !== 16n ||
    binary !== 16n
) {
    throw "non-decimal BigInt mismatch";
}
process.stdout.write("ok\n");
"#,
    );

    let jit = project
        .command()
        .args(["run", "--target", "jit", "main.ts"])
        .current_dir(&project.path)
        .output()
        .expect("bamts JIT BigInt program starts");
    assert_success(&jit, "bamts JIT BigInt program");

    let aot = project
        .command()
        .args(["run", "--target", "aot", "main.ts"])
        .current_dir(&project.path)
        .output()
        .expect("bamts AOT BigInt program starts");
    assert_success(&aot, "bamts AOT BigInt program");

    assert_eq!(jit.stdout, b"ok\n");
    assert_eq!(aot.stdout, jit.stdout);
}

#[test]
fn aot_and_jit_execute_classic_script_dynamic_imports() {
    let project = ScratchDirectory::new();
    project.write("main.ts", CLASSIC_DYNAMIC_IMPORT_PROGRAM);

    let jit = project
        .command()
        .args(["run", "--target", "jit", "main.ts"])
        .current_dir(&project.path)
        .output()
        .expect("bamts JIT classic dynamic import program starts");
    assert_success(&jit, "bamts JIT classic dynamic import program");

    let aot = project
        .command()
        .args(["run", "--target", "aot", "main.ts"])
        .current_dir(&project.path)
        .output()
        .expect("bamts AOT classic dynamic import program starts");
    assert_success(&aot, "bamts AOT classic dynamic import program");

    assert_eq!(jit.stdout, b"function\n");
    assert_eq!(aot.stdout, jit.stdout);
}

#[test]
fn aot_and_jit_run_all_nested_finally_completions() {
    let project = ScratchDirectory::new();
    project.write(
        "main.ts",
        r#"const trace: string[] = [];
function returnProbe(): number {
    try {
        try { return 7; } finally { trace.push("return-inner"); }
    } finally {
        trace.push("return-outer");
    }
}
trace.push("return:" + returnProbe());
try {
    try {
        try { throw "boom"; } finally { trace.push("throw-inner"); }
    } finally {
        trace.push("throw-outer");
    }
} catch (error) {
    trace.push("catch:" + error);
}
for (;;) {
    try {
        try { trace.push("break-body"); break; } finally { trace.push("break-inner"); }
    } finally {
        trace.push("break-outer");
    }
}
for (let i = 0; i < 2; i++) {
    try {
        try { trace.push("continue-body:" + i); continue; }
        finally { trace.push("continue-inner:" + i); }
    } finally {
        trace.push("continue-outer:" + i);
    }
}
process.stdout.write(trace.join(",") + "\n");
"#,
    );

    let expected = b"return-inner,return-outer,return:7,throw-inner,throw-outer,catch:boom,\
break-body,break-inner,break-outer,continue-body:0,continue-inner:0,continue-outer:0,\
continue-body:1,continue-inner:1,continue-outer:1\n";
    for target in ["jit", "aot"] {
        let output = project
            .command()
            .args(["run", "--target", target, "main.ts"])
            .current_dir(&project.path)
            .output()
            .unwrap_or_else(|error| {
                panic!("bamts {target} nested-finally program starts: {error}")
            });
        assert_success(&output, &format!("bamts {target} nested-finally program"));
        assert_eq!(output.stdout, expected, "{target}");
    }
}

#[test]
fn aot_and_jit_run_labeled_control_flow() {
    let project = ScratchDirectory::new();
    project.write(
        "main.ts",
        r#"const trace: string[] = [];
block: {
    trace.push("block");
    break block;
    trace.push("bad-block");
}
trace.push("after-block");
let visits = 0;
first: second: for (let i = 0; i < 3; i++) {
    visits++;
    if (i < 2) continue first;
    trace.push("loop:" + i);
}
trace.push("visits:" + visits);
exit: {
    try {
        try { trace.push("break-body"); break exit; }
        finally { trace.push("break-inner"); }
    } finally {
        trace.push("break-outer");
    }
    trace.push("bad-exit");
}
outer: for (let i = 0; i < 2; i++) {
    try {
        try { trace.push("continue-body:" + i); continue outer; }
        finally { trace.push("continue-inner:" + i); }
    } finally {
        trace.push("continue-outer:" + i);
    }
}
crossed: for (let i = 0; i < 2; i++) {
    try {
        for (let j = 0; j < 2; j++) {
            try { break crossed; } finally { trace.push("crossed-inner:" + i); }
        }
        trace.push("bad-crossed:" + i);
    } finally {
        trace.push("crossed-outer:" + i);
    }
}
choice: switch (1) {
    case 1: trace.push("switch"); break choice;
    default: trace.push("bad-switch");
}
process.stdout.write(trace.join(",") + "\n");
"#,
    );

    let expected = b"block,after-block,loop:2,visits:3,break-body,break-inner,break-outer,\
continue-body:0,continue-inner:0,continue-outer:0,continue-body:1,continue-inner:1,\
continue-outer:1,crossed-inner:0,crossed-outer:0,switch\n";
    for target in ["jit", "aot"] {
        let output = project
            .command()
            .args(["run", "--target", target, "main.ts"])
            .current_dir(&project.path)
            .output()
            .unwrap_or_else(|error| panic!("bamts {target} labeled program starts: {error}"));
        assert_success(&output, &format!("bamts {target} labeled program"));
        assert_eq!(output.stdout, expected, "{target}");
    }
}

#[test]
fn aot_and_jit_close_iterators_after_finally_and_preserve_body_throw() {
    let project = ScratchDirectory::new();
    project.write(
        "main.ts",
        r#"const trace: string[] = [];
const iterable = {
    [Symbol.iterator]() {
        return {
            next() { return { value: 1, done: false }; },
            return() {
                trace.push("return()");
                throw "close-error";
            },
        };
    },
};
try {
    for (const value of iterable) {
        try {
            trace.push("body:" + value);
            throw "body-error";
        } finally {
            trace.push("finally");
        }
    }
} catch (error) {
    trace.push("catch:" + error);
}
process.stdout.write(trace.join(",") + "\n");
"#,
    );

    let expected = b"body:1,finally,return(),catch:body-error\n";
    for target in ["jit", "aot"] {
        let output = project
            .command()
            .args(["run", "--target", target, "main.ts"])
            .current_dir(&project.path)
            .output()
            .unwrap_or_else(|error| {
                panic!("bamts {target} iterator-close ordering program starts: {error}")
            });
        assert_success(
            &output,
            &format!("bamts {target} iterator-close ordering program"),
        );
        assert_eq!(output.stdout, expected, "{target}");
    }
}

#[test]
fn aot_and_jit_share_escaped_identifier_identity() {
    let project = ScratchDirectory::new();
    project.write("dependency.ts", "export const \\u0076alue = 41;\n");
    project.write(
        "main.ts",
        r#"import { value as \u0061lias } from "./dependency.ts";
const increment = (\u{0000006e}: number) => n + 1;
process.stdout.write(String(increment(alias)) + "\n");
"#,
    );

    for target in ["jit", "aot"] {
        let output = project
            .command()
            .args(["run", "--target", target, "main.ts"])
            .current_dir(&project.path)
            .output()
            .unwrap_or_else(|error| panic!("bamts {target} escaped-name program starts: {error}"));
        assert_success(&output, &format!("bamts {target} escaped-name program"));
        assert_eq!(output.stdout, b"42\n", "{target}");
    }
}

#[test]
fn aot_and_jit_keep_non_exported_nested_namespaces_local() {
    let project = ScratchDirectory::new();
    project.write(
        "main.ts",
        r#"namespace A {
    namespace B {
        export const x = 1;
    }
    export namespace C {
        export const x = 2;
    }
    export const localValue = B.x;
}
if (A.localValue !== 1 || "B" in A || A.C.x !== 2) {
    throw "nested namespace visibility mismatch";
}
process.stdout.write("ok\n");
"#,
    );

    for target in ["jit", "aot"] {
        let output = project
            .command()
            .args(["run", "--target", target, "main.ts"])
            .current_dir(&project.path)
            .output()
            .unwrap_or_else(|error| {
                panic!("bamts {target} nested-namespace program starts: {error}")
            });
        assert_success(&output, &format!("bamts {target} nested-namespace program"));
        assert_eq!(output.stdout, b"ok\n", "{target}");
    }
}

#[test]
fn aot_and_jit_share_one_live_global_object() {
    let project = ScratchDirectory::new();
    project.write(
        "main.ts",
        r#"const root: any = globalThis;
const stdout = process.stdout;
const originalConsole = console;
const replacementConsole = { marker: 1 };

root.console = replacementConsole;
if (console !== replacementConsole) throw "property-to-identifier mismatch";
console = originalConsole;
if (root.console !== originalConsole) throw "identifier-to-property mismatch";

let lexicalBinding = 14;
root.lexicalBinding = 99;
if (lexicalBinding !== 14 || root.lexicalBinding !== 99) {
    throw "module binding leaked into the global object";
}

root.globalThis = { fake: true };
root.global = { fake: true };
console = replacementConsole;
if (root.console !== replacementConsole) throw "global alias redirected storage";
delete root.globalThis;
delete root.global;
console = originalConsole;
if (root.console !== originalConsole) throw "global alias deletion redirected storage";

root.removable = 1;
if (!delete root.removable || "removable" in root) {
    throw "global property deletion mismatch";
}

let ownReceiver = false;
let ownSet: unknown;
Object.defineProperty(root, "console", {
    get() { return replacementConsole; },
    set(value) { ownReceiver = this === root; ownSet = value; },
    configurable: true,
});
if (console !== replacementConsole) throw "own global getter mismatch";
console = originalConsole;
if (!ownReceiver || ownSet !== originalConsole) throw "own global setter mismatch";
Object.defineProperty(root, "console", {
    value: originalConsole,
    writable: true,
    configurable: true,
});

const originalQueueMicrotask = queueMicrotask;
delete root.queueMicrotask;
let prototypeReceiver = false;
let prototypeSet: unknown;
Object.defineProperty(Object.prototype, "queueMicrotask", {
    get() { return originalQueueMicrotask; },
    set(value) { prototypeReceiver = this === root; prototypeSet = value; },
    configurable: true,
});
if (queueMicrotask !== originalQueueMicrotask) throw "prototype global getter mismatch";
queueMicrotask = originalQueueMicrotask;
if (!prototypeReceiver || prototypeSet !== originalQueueMicrotask) {
    throw "prototype global setter mismatch";
}
delete Object.prototype.queueMicrotask;
root.queueMicrotask = originalQueueMicrotask;

let infinityFailed = false;
let nanFailed = false;
try { root.Infinity = 1; } catch { infinityFailed = true; }
try { root.NaN = 1; } catch { nanFailed = true; }
if (!infinityFailed || !nanFailed || root.Infinity !== Infinity || root.NaN === root.NaN) {
    throw "frozen global constant mismatch";
}

stdout.write("ok\n");
"#,
    );

    for target in ["jit", "aot"] {
        let output = project
            .command()
            .args(["run", "--target", target, "main.ts"])
            .current_dir(&project.path)
            .output()
            .unwrap_or_else(|error| panic!("bamts {target} global-object program starts: {error}"));
        assert_success(&output, &format!("bamts {target} global-object program"));
        assert_eq!(output.stdout, b"ok\n", "{target}");
    }
}

#[test]
fn aot_and_jit_preserve_vm_context_for_escaped_closures() {
    let project = ScratchDirectory::new();
    project.write(
        "main.ts",
        r#"import { runInNewContext } from "node:vm";

const context: any = { secret: 41 };
const readSecret: any = runInNewContext(
    "(function () { return secret + 1; })",
    context,
);
context.secret = 99;
if (readSecret() !== 100) {
    throw "escaped closure lost its VM context";
}
process.stdout.write("ok\n");
"#,
    );

    for target in ["jit", "aot"] {
        let output = project
            .command()
            .args(["run", "--target", target, "main.ts"])
            .current_dir(&project.path)
            .output()
            .unwrap_or_else(|error| panic!("bamts {target} VM-context program starts: {error}"));
        assert_success(&output, &format!("bamts {target} VM-context program"));
        assert_eq!(output.stdout, b"ok\n", "{target}");
    }
}

#[test]
fn aot_fixture_matches_jit_stdout_and_exit_code() {
    let directory = ScratchDirectory::new();
    let executable = directory
        .path
        .join(format!("hello{}", std::env::consts::EXE_SUFFIX));
    let compile = directory
        .command()
        .args(["compile", "--target", "aot", "--output"])
        .arg(&executable)
        .arg(fixture())
        .output()
        .expect("bamts compile starts");
    assert_success(&compile, "bamts compile --target aot");

    let output = Command::new(&executable)
        .output()
        .expect("compiled executable starts");
    assert_success(&output, "compiled fixture");
    assert_eq!(output.stdout, EXPECTED_STDOUT);
}

#[test]
fn jit_runs_two_module_program_with_live_imported_mutation() {
    let project = ScratchDirectory::new();
    project.write("dependency.ts", "export let value = 1; value = 2;\n");
    project.write(
        "main.ts",
        "import { value } from './dependency.js'; console.log(value);\n",
    );

    let output = project
        .command()
        .args(["run", "--target", "jit", "main.ts"])
        .current_dir(&project.path)
        .output()
        .expect("bamts JIT run starts");

    assert_success(&output, "bamts run two-module JIT");
    assert_eq!(output.stdout, b"2\n");
}

#[test]
fn aot_runs_two_module_program_with_live_imported_mutation() {
    let project = ScratchDirectory::new();
    project.write("dependency.ts", "export let value = 1; value = 2;\n");
    project.write(
        "main.ts",
        "import { value } from './dependency.js'; console.log(value);\n",
    );
    let executable = project
        .path
        .join(format!("two-module{}", std::env::consts::EXE_SUFFIX));

    let compile = project
        .command()
        .args(["compile", "--target", "aot", "--output"])
        .arg(&executable)
        .arg("main.ts")
        .current_dir(&project.path)
        .output()
        .expect("bamts AOT compile starts");
    assert_success(&compile, "bamts compile two-module AOT");

    let output = Command::new(&executable)
        .output()
        .expect("two-module AOT executable starts");
    assert_success(&output, "compiled two-module program");
    assert_eq!(output.stdout, b"2\n");
}

#[test]
fn jit_preserves_lone_surrogates_end_to_end() {
    let project = ScratchDirectory::new();
    project.write("main.ts", UTF16_PROGRAM);

    let output = project
        .command()
        .args(["run", "--target", "jit", "main.ts"])
        .current_dir(&project.path)
        .output()
        .expect("bamts JIT run starts");

    assert_success(&output, "bamts run UTF-16 JIT");
    assert_eq!(output.stdout, UTF16_STDOUT);
}

#[test]
fn aot_preserves_lone_surrogates_end_to_end() {
    let project = ScratchDirectory::new();
    project.write("main.ts", UTF16_PROGRAM);
    let executable = project
        .path
        .join(format!("utf16{}", std::env::consts::EXE_SUFFIX));

    let compile = project
        .command()
        .args(["compile", "--target", "aot", "--output"])
        .arg(&executable)
        .arg("main.ts")
        .current_dir(&project.path)
        .output()
        .expect("bamts AOT compile starts");
    assert_success(&compile, "bamts compile UTF-16 AOT");

    let output = Command::new(&executable)
        .output()
        .expect("UTF-16 AOT executable starts");
    assert_success(&output, "compiled UTF-16 program");
    assert_eq!(output.stdout, UTF16_STDOUT);
}

#[test]
fn jit_supports_apply_and_bound_callables() {
    let project = ScratchDirectory::new();
    project.write("main.ts", CALLABLE_PROGRAM);

    let output = project
        .command()
        .args(["run", "--target", "jit", "main.ts"])
        .current_dir(&project.path)
        .output()
        .expect("bamts callable JIT run starts");

    assert_success(&output, "bamts run callable JIT");
    assert!(output.stdout.is_empty());
}

#[test]
fn aot_supports_apply_and_bound_callables() {
    let project = ScratchDirectory::new();
    project.write("main.ts", CALLABLE_PROGRAM);
    let executable = project
        .path
        .join(format!("callable{}", std::env::consts::EXE_SUFFIX));

    let compile = project
        .command()
        .args(["compile", "--target", "aot", "--output"])
        .arg(&executable)
        .arg("main.ts")
        .current_dir(&project.path)
        .output()
        .expect("bamts callable AOT compile starts");
    assert_success(&compile, "bamts compile callable AOT");

    let output = Command::new(&executable)
        .output()
        .expect("callable AOT executable starts");
    assert_success(&output, "compiled callable program");
    assert!(output.stdout.is_empty());
}

#[test]
fn aot_runs_node_vm_in_new_context() {
    let project = ScratchDirectory::new();
    project.write("main.ts", VM_PROGRAM);
    let executable = project
        .path
        .join(format!("node-vm{}", std::env::consts::EXE_SUFFIX));

    let compile = project
        .command()
        .args(["compile", "--target", "aot", "--output"])
        .arg(&executable)
        .arg("main.ts")
        .current_dir(&project.path)
        .output()
        .expect("bamts node:vm AOT compile starts");
    assert_success(&compile, "bamts compile node:vm AOT");

    let output = Command::new(&executable)
        .output()
        .expect("node:vm AOT executable starts");
    assert_success(&output, "compiled node:vm program");
    assert!(
        !stderr(&output).contains("bamts: aot runtime"),
        "{}",
        stderr(&output)
    );
    assert_eq!(output.stdout, b"2\nobject\n");
}

#[test]
fn jit_runs_external_import_equals_with_one_binding() {
    let project = ScratchDirectory::new();
    project.write(
        "main.ts",
        r#"import util = require('node:util');
process.stdout.write(String(typeof util.parseArgs) + "\n");
"#,
    );

    let output = project
        .command()
        .args(["run", "--target", "jit", "main.ts"])
        .current_dir(&project.path)
        .output()
        .expect("bamts JIT import-equals run starts");

    assert_success(&output, "bamts JIT import-equals run");
    assert_eq!(output.stdout, b"function\n");
}

#[test]
fn jit_runs_local_commonjs_style_import_equals_with_one_binding() {
    let project = ScratchDirectory::new();
    project.write("dependency.ts", "export const value = 41;\n");
    project.write(
        "main.ts",
        r#"import dependency = require('./dependency.js');
process.stdout.write(String(dependency.value + 1) + "\n");
"#,
    );

    let output = project
        .command()
        .args(["run", "--target", "jit", "main.ts"])
        .current_dir(&project.path)
        .output()
        .expect("bamts JIT local import-equals run starts");

    assert_success(&output, "bamts JIT local import-equals run");
    assert_eq!(output.stdout, b"42\n");
}

#[test]
fn check_reports_dependency_errors() {
    let project = ScratchDirectory::new();
    project.write("main.ts", "import './dependency.ts';\n");
    project.write("dependency.ts", "const = 1;\n");

    let output = project.check("main.ts");

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("dependency.ts"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn check_loads_type_only_dependencies() {
    let project = ScratchDirectory::new();
    project.write(
        "main.ts",
        "import type { Shape } from './types.ts';\nexport const loaded = true;\n",
    );
    project.write("types.ts", "export interface Shape { value: string }\n");

    assert_success(&project.check("main.ts"), "bamts check type-only graph");
}

#[test]
fn check_loads_diamond_graph_once() {
    let project = ScratchDirectory::new();
    project.write("main.ts", "import './left.ts';\nimport './right.ts';\n");
    project.write("left.ts", "import './leaf.ts';\nexport const left = 1;\n");
    project.write("right.ts", "import './leaf.ts';\nexport const right = 2;\n");
    project.write("leaf.ts", "export const leaf = 3;\n");

    assert_success(&project.check("main.ts"), "bamts check diamond graph");
}

#[test]
fn check_accepts_module_cycles() {
    let project = ScratchDirectory::new();
    project.write("main.ts", "import './a.ts';\n");
    project.write("a.ts", "import './b.ts';\nexport const a = 1;\n");
    project.write("b.ts", "import './a.ts';\nexport const b = 2;\n");

    assert_success(&project.check("main.ts"), "bamts check cyclic graph");
}

#[test]
fn check_applies_project_lint_config_to_dependencies() {
    let project = ScratchDirectory::new();
    project.write("bamts.toml", "[lints.rules]\nexplicit-any = \"deny\"\n");
    project.write("src/tsconfig.json", "{}\n");
    project.write("src/main.ts", "import '../dependency.ts';\n");
    project.write("dependency.ts", "export const value: any = 1;\n");

    let output = project.check_from("src", "main.ts");

    assert!(!output.status.success());
    let stderr = stderr(&output);
    assert!(stderr.contains("dependency.ts"), "{stderr}");
    assert!(stderr.contains("BAMTS-W017"), "{stderr}");
}

#[test]
fn check_renders_multi_file_diagnostics_in_stable_source_order() {
    let project = ScratchDirectory::new();
    project.write("main.ts", "import './first.ts';\nimport './second.ts';\n");
    project.write("first.ts", "const = 1;\n");
    project.write("second.ts", "const = 2;\n");

    let first = project.check("main.ts");
    let second = project.check("main.ts");

    assert!(!first.status.success());
    assert_eq!(first.stderr, second.stderr);
    let stderr = stderr(&first);
    let first_position = stderr.find("first.ts").expect("first diagnostic");
    let second_position = stderr.find("second.ts").expect("second diagnostic");
    assert!(first_position < second_position, "{stderr}");
}

fn bamts_binary() -> &'static str {
    env!("CARGO_BIN_EXE_bamts")
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hello.ts")
}

fn assert_success(output: &Output, command: &str) {
    assert!(
        output.status.success(),
        "{command} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

struct ScratchDirectory {
    path: PathBuf,
}

impl ScratchDirectory {
    fn new() -> Self {
        let root = std::env::temp_dir();
        for _ in 0..128 {
            let index = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!("bamts-cli-test-{}-{index}", std::process::id()));
            #[cfg(unix)]
            let created = {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new().mode(0o700).create(&path)
            };
            #[cfg(not(unix))]
            let created = fs::create_dir(&path);
            match created {
                Ok(()) => return Self { path },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create `{}`: {error}", path.display()),
            }
        }
        panic!("could not allocate a unique CLI test directory");
    }

    fn write(&self, relative: &str, source: &str) {
        let path = self.path.join(relative);
        fs::create_dir_all(path.parent().expect("fixture path has a parent"))
            .expect("fixture directory is created");
        fs::write(path, source).expect("fixture source is written");
    }

    fn command(&self) -> Command {
        let mut command = Command::new(bamts_binary());
        command.env("BAMTS_CACHE_DIR", self.path.join("cache"));
        command
    }

    fn check(&self, entrypoint: &str) -> Output {
        self.command()
            .args(["check", "--diagnostics-format", "text", entrypoint])
            .current_dir(&self.path)
            .output()
            .expect("bamts check starts")
    }

    fn check_from(&self, directory: &str, entrypoint: &str) -> Output {
        self.command()
            .args(["check", "--diagnostics-format", "text", entrypoint])
            .current_dir(self.path.join(directory))
            .output()
            .expect("bamts check starts")
    }
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
