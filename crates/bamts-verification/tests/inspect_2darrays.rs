use bamts_verification::check_cells::{
    compile_case, emit_types_baseline, entry_virtual_path, split_case_units,
};

#[test]
fn inspect_2darrays_emitted_types() {
    let source = "// @target: es2015\nclass Cell {\n}\n\nclass Ship {\n    isSunk: boolean = false;\n}\n\nclass Board {\n    ships: Ship[] = [];\n    cells: Cell[] = [];\n\n    private allShipsSunk() {\n        return this.ships.every(function (val) { return val.isSunk; });\n    }    \n}\n";
    let logical = "tests/cases/compiler/2dArrays.ts";
    let units = split_case_units(logical, source);
    let entry = entry_virtual_path(logical, &units);
    let case = compile_case(&units, &entry).expect("compile");
    let emitted = emit_types_baseline(&case, logical);
    println!("{}", emitted);
}
