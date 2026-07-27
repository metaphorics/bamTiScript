use std::{
    collections::{BTreeMap, BTreeSet},
    env, fmt, fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{
    Deserialize,
    de::{Deserializer, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{ErrorCode, Gate, GateReport, Result, VerificationError};

const QUINT_VERSION: &str = "0.32.0";
const RACKET_VERSION: &str = "Racket v9.2 [cs]";
const QUINT_RUNS_PATH: &str = "verification/formal/quint/runs.json";
const QUINT_MUTATIONS_PATH: &str = "proof/quint-mutations.json";
const QUINT_WITNESSES_PATH: &str = "proof/quint-normal-witnesses.json";
const LEAN_ASSUMPTIONS_PATH: &str = "proof/lean-assumptions.json";
const MANIFEST_PATH: &str = "verification/manifest.lock.json";
const OUTPUT_PREVIEW_LIMIT: usize = 4096;
const EXPECTED_LIVENESS_WITNESSES: usize = 2;
const STANDARD_LEAN_ASSUMPTIONS: [&str; 3] = ["Classical.choice", "Quot.sound", "propext"];

/// Audits the executable formal gates owned by this module.
///
/// The caller may present a dependency-closed gate set in display order.  This
/// function executes the requested gates in [`Gate::FORMAL_ORDER`] and returns
/// reports in that execution order.  G0 and G6 deliberately belong to their
/// separate owners.
pub fn audit_formal_gates(root: &Path, gates: &[Gate]) -> Result<Vec<GateReport>> {
    audit_formal_gates_with_tools(root, gates, &ToolOverrides::default())
}

fn audit_formal_gates_with_tools(
    root: &Path,
    gates: &[Gate],
    overrides: &ToolOverrides,
) -> Result<Vec<GateReport>> {
    let ordered = ordered_requested_gates(gates)?;
    let mut reports = Vec::with_capacity(ordered.len());

    for gate in ordered {
        let checks = match gate {
            Gate::G1 => audit_g1(root, overrides)?,
            Gate::G2 => audit_g2(root, overrides)?,
            Gate::G3 => audit_g3(root, overrides)?,
            Gate::G4 => audit_g4(root)?,
            Gate::G5 => audit_g5(root, overrides)?,
            Gate::G0 | Gate::G6 => {
                return Err(VerificationError::new(
                    ErrorCode::Usage,
                    "formal_gates only owns G1 through G5",
                ));
            }
        };
        reports.push(GateReport { gate, checks });
    }

    Ok(reports)
}

fn ordered_requested_gates(gates: &[Gate]) -> Result<Vec<Gate>> {
    if gates.is_empty() {
        return Err(VerificationError::new(
            ErrorCode::Usage,
            "formal gate audit requires at least one of G1 through G5",
        ));
    }

    let order: Vec<Gate> = Gate::FORMAL_ORDER
        .into_iter()
        .filter(|gate| *gate != Gate::G6)
        .collect();
    let mut requested = BTreeSet::new();
    let mut highest = None;

    for gate in gates {
        if !order.contains(gate) {
            return Err(VerificationError::new(
                ErrorCode::Usage,
                format!("unsupported formal gate {gate:?}; formal_gates owns G1 through G5"),
            ));
        }
        if !requested.insert(*gate) {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("duplicate formal gate {gate:?}"),
            ));
        }
        let position = order
            .iter()
            .position(|candidate| candidate == gate)
            .ok_or_else(|| {
                VerificationError::new(
                    ErrorCode::Usage,
                    format!("unsupported formal gate {gate:?}"),
                )
            })?;
        highest = Some(highest.map_or(position, |current: usize| current.max(position)));
    }

    let highest = highest
        .ok_or_else(|| VerificationError::new(ErrorCode::Usage, "no formal gates requested"))?;
    let required: BTreeSet<_> = order.iter().take(highest + 1).copied().collect();
    if requested != required {
        return Err(VerificationError::new(
            ErrorCode::GateDependency,
            "requested formal gates are not a dependency-closed prefix of Gate::FORMAL_ORDER",
        ));
    }

    Ok(order
        .into_iter()
        .filter(|gate| requested.contains(gate))
        .collect())
}

fn audit_g1(root: &Path, overrides: &ToolOverrides) -> Result<usize> {
    let quint_models = collect_sources(&root.join("formal/quint"), "qnt", true)?;
    if quint_models.is_empty() {
        return Err(schema_error(root, "formal/quint contains no Quint models"));
    }
    let redex_sources = collect_sources(&root.join("formal/redex"), "rkt", false)?;
    if redex_sources.is_empty() {
        return Err(schema_error(
            root,
            "formal/redex contains no Racket sources",
        ));
    }

    let quint = quint_launcher(root, overrides)?;
    let mut checks = 0usize;
    let version = run_tool(root, &quint, &["--version".into()], "local Quint version")?;
    if String::from_utf8_lossy(&version.stdout).trim() != QUINT_VERSION {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!(
                "local Quint launcher {} did not report {QUINT_VERSION}: {}",
                quint.display(),
                bounded_child_output(&version)
            ),
        ));
    }
    increment(&mut checks)?;

    for model in quint_models {
        let relative = root_relative(root, &model)?;
        run_tool(
            root,
            &quint,
            &["typecheck".into(), relative.into_os_string()],
            &format!("Quint typecheck {}", model.display()),
        )?;
        increment(&mut checks)?;
    }

    let (raco, racket) = racket_tools(overrides)?;
    verify_racket_pin(root, &racket)?;
    increment(&mut checks)?;
    for source in redex_sources {
        let relative = root_relative(root, &source)?;
        run_tool(
            root,
            &raco,
            &["make".into(), relative.into_os_string()],
            &format!("raco make {}", source.display()),
        )?;
        increment(&mut checks)?;
    }

    let lean_root = rooted_directory(root, "formal/lean")?;
    let lake = lake_tool(overrides)?;
    run_tool(&lean_root, &lake, &["build".into()], "lake build")?;
    increment(&mut checks)?;
    Ok(checks)
}

fn audit_g2(root: &Path, overrides: &ToolOverrides) -> Result<usize> {
    let manifest = read_quint_runs(root)?;
    let models = load_quint_models(root)?;
    validate_quint_runs(root, &manifest, &models)?;
    let quint = quint_launcher(root, overrides)?;
    let mut checks = 0usize;

    for run in &manifest.runs {
        let model = rooted_qnt_path(root, &run.model)?;
        let mut arguments = vec![
            "run".into(),
            root_relative(root, &model)?.into_os_string(),
            "--main".into(),
            run.main_module.clone().into(),
            "--init".into(),
            run.init.clone().into(),
            "--step".into(),
            run.step.clone().into(),
            "--backend".into(),
            "rust".into(),
            "--n-threads".into(),
            "1".into(),
            "--max-steps".into(),
            run.max_steps.to_string().into(),
            "--max-samples".into(),
            run.max_samples.to_string().into(),
            "--seed".into(),
            run.seed.clone().into(),
            "--invariants".into(),
        ];
        arguments.extend(run.invariants.iter().cloned().map(Into::into));
        run_tool(
            root,
            &quint,
            &arguments,
            &format!("Quint safety simulation {}", run.model),
        )?;
        increment(&mut checks)?;

        if let Some(witness) = &run.liveness_witness {
            let arguments = vec![
                "run".into(),
                root_relative(root, &model)?.into_os_string(),
                "--main".into(),
                run.main_module.clone().into(),
                "--init".into(),
                run.init.clone().into(),
                "--step".into(),
                run.step.clone().into(),
                "--backend".into(),
                "rust".into(),
                "--n-threads".into(),
                "1".into(),
                "--max-steps".into(),
                witness.max_steps.to_string().into(),
                "--max-samples".into(),
                witness.max_samples.to_string().into(),
                "--seed".into(),
                witness.seed.clone().into(),
                "--witnesses".into(),
                witness.name.clone().into(),
            ];
            let output = run_tool(
                root,
                &quint,
                &arguments,
                &format!("Quint liveness witness {}::{}", run.model, witness.name),
            )?;
            validate_quint_liveness_output(&output, witness)?;
            increment(&mut checks)?;
        }
    }

    for (model, source) in &models {
        if source.runs.is_empty() {
            continue;
        }
        let path = rooted_qnt_path(root, model)?;
        run_tool(
            root,
            &quint,
            &[
                "test".into(),
                root_relative(root, &path)?.into_os_string(),
                "--backend".into(),
                "rust".into(),
            ],
            &format!("Quint test {model}"),
        )?;
        increment(&mut checks)?;
    }

    Ok(checks)
}

fn audit_g3(root: &Path, overrides: &ToolOverrides) -> Result<usize> {
    let catalog = formal_catalog(root, "formal-lean")?;
    let assumptions = read_lean_assumptions(root)?;
    let theorems = validate_lean_assumptions(root, &catalog, &assumptions)?;

    let lean_root = rooted_directory(root, "formal/lean")?;
    let lake = lake_tool(overrides)?;
    let mut checks = 0usize;
    run_tool(&lean_root, &lake, &["build".into()], "lake build for G3")?;
    increment(&mut checks)?;

    let temp = LeanAxiomInput::create(&lean_root, &theorems)?;
    let output = run_tool(
        &lean_root,
        &lake,
        &[
            "env".into(),
            "lean".into(),
            temp.path().as_os_str().to_os_string(),
        ],
        "lake env lean axiom audit",
    )?;
    increment(&mut checks)?;
    let observed = parse_lean_axioms(&output, &theorems)?;
    let allowed = string_set(&STANDARD_LEAN_ASSUMPTIONS);
    let mut union = BTreeSet::new();
    for (name, axioms) in observed {
        if !axioms.is_subset(&allowed) {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!("Lean theorem {name} has an unexpected kernel assumption"),
            ));
        }
        union.extend(axioms);
        increment(&mut checks)?;
    }
    if union != allowed {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            "Lean axiom audit did not observe exactly the approved kernel assumption set",
        ));
    }
    Ok(checks)
}

fn audit_g4(root: &Path) -> Result<usize> {
    let mut checks = audit_forbidden_material(root)?;

    let quint_catalog = formal_catalog(root, "formal-quint")?;
    let lean_catalog = formal_catalog(root, "formal-lean")?;
    let redex_catalog = formal_catalog(root, "formal-redex")?;
    checks = checked_add(
        checks,
        validate_quint_catalog_authority(root, &quint_catalog)?,
    )?;
    checks = checked_add(
        checks,
        validate_lean_catalog_authority(root, &lean_catalog)?,
    )?;
    checks = checked_add(
        checks,
        validate_redex_catalog_authority(root, &redex_catalog)?,
    )?;
    checks = checked_add(checks, validate_quint_controls(root, &quint_catalog)?)?;
    Ok(checks)
}

fn audit_g5(root: &Path, overrides: &ToolOverrides) -> Result<usize> {
    let (raco, racket) = racket_tools(overrides)?;
    verify_racket_pin(root, &racket)?;
    run_tool(
        root,
        &raco,
        &[
            "test".into(),
            "--process".into(),
            "--check-stderr".into(),
            "--timeout".into(),
            "120".into(),
            "formal/redex".into(),
        ],
        "raco test formal/redex",
    )?;
    Ok(2)
}

fn validate_quint_controls(root: &Path, catalog: &FormalCatalog) -> Result<usize> {
    let runs = read_quint_runs(root)?;
    let models = load_quint_models(root)?;
    validate_quint_runs(root, &runs, &models)?;
    let liveness = liveness_bindings(&runs, &catalog.identifiers)?;
    validate_simulated_catalog_coverage(&runs, catalog, &liveness)?;
    let mutations = read_mutations(root)?;
    let witnesses = read_normal_witnesses(root)?;
    validate_control_documents(root, catalog, &models, &liveness, &mutations, &witnesses)
}

fn validate_control_documents(
    root: &Path,
    catalog: &FormalCatalog,
    models: &BTreeMap<String, QuintModel>,
    liveness: &BTreeMap<String, LivenessBinding>,
    mutations: &MutationDocument,
    witnesses: &NormalWitnessDocument,
) -> Result<usize> {
    validate_control_tool(&mutations.tool, QUINT_MUTATIONS_PATH)?;
    if mutations.schema != "bamti.quint-mutations/v2" {
        return Err(schema_error(
            &root.join(QUINT_MUTATIONS_PATH),
            "mutation manifest must use bamti.quint-mutations/v2",
        ));
    }
    validate_control_tool(&witnesses.tool, QUINT_WITNESSES_PATH)?;
    if witnesses.schema != "bamti.quint-normal-witnesses/v1" {
        return Err(schema_error(
            &root.join(QUINT_WITNESSES_PATH),
            "normal witness manifest must use bamti.quint-normal-witnesses/v1",
        ));
    }

    let mut safety = catalog.identifiers.clone();
    for liveness_id in liveness.keys() {
        if !safety.remove(liveness_id) {
            return Err(VerificationError::new(
                ErrorCode::SetMismatch,
                format!("liveness witness {liveness_id} is not in the formal-Quint catalog"),
            ));
        }
    }

    let mut actual_mutations = BTreeSet::new();
    let mut checks = 0usize;
    for mutation in &mutations.mutations {
        validate_simulation_controls(
            &mutation.main_module,
            &mutation.init,
            &mutation.step,
            &mutation.backend,
            mutation.threads,
            mutation.max_steps,
            mutation.max_samples,
            &mutation.seed,
            &root.join(QUINT_MUTATIONS_PATH),
        )?;
        let invariant_id = catalog_id(
            &mutation.model,
            &mutation.invariant,
            &root.join(QUINT_MUTATIONS_PATH),
        )?;
        if !actual_mutations.insert(invariant_id.clone()) {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("duplicate mutation control for {invariant_id}"),
            ));
        }
        let fault_model = models.get(&mutation.fault_model).ok_or_else(|| {
            schema_error(
                &root.join(QUINT_MUTATIONS_PATH),
                format!(
                    "fault model {} is not a formal Quint source",
                    mutation.fault_model
                ),
            )
        })?;
        require_model_module(
            fault_model,
            &mutation.main_module,
            &root.join(QUINT_MUTATIONS_PATH),
        )?;
        require_identifier(
            &mutation.fault_action,
            "fault action",
            &root.join(QUINT_MUTATIONS_PATH),
        )?;
        require_identifier(
            &mutation.fault_run,
            "fault run",
            &root.join(QUINT_MUTATIONS_PATH),
        )?;
        if !mutation.fault_action.starts_with("fault") {
            return Err(schema_error(
                &root.join(QUINT_MUTATIONS_PATH),
                format!(
                    "fault action {} is not an injected fault action",
                    mutation.fault_action
                ),
            ));
        }
        require_exact_declaration(
            &fault_model.actions,
            &mutation.fault_action,
            "fault action",
            &root.join(QUINT_MUTATIONS_PATH),
        )?;
        let fault_run = require_exact_run(
            fault_model,
            &mutation.fault_run,
            &root.join(QUINT_MUTATIONS_PATH),
        )?;
        if !fault_run.contains(&mutation.fault_action)
            || !fault_run.contains(&mutation.invariant)
            || !fault_run.contains("not(")
        {
            return Err(schema_error(
                &root.join(QUINT_MUTATIONS_PATH),
                format!(
                    "fault run {} is not a passing kill control for {}",
                    mutation.fault_run, invariant_id
                ),
            ));
        }
        increment(&mut checks)?;
    }
    require_same_set("mutation controls", &safety, &actual_mutations)?;

    let mut actual_witnesses = BTreeSet::new();
    for witness in &witnesses.witnesses {
        validate_simulation_controls(
            &witness.main_module,
            &witness.init,
            &witness.step,
            &witness.backend,
            witness.threads,
            witness.max_steps,
            witness.max_samples,
            &witness.seed,
            &root.join(QUINT_WITNESSES_PATH),
        )?;
        let invariant_id = catalog_id(
            &witness.model,
            &witness.invariant,
            &root.join(QUINT_WITNESSES_PATH),
        )?;
        if !actual_witnesses.insert(invariant_id.clone()) {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("duplicate normal witness control for {invariant_id}"),
            ));
        }
        let witness_model = models.get(&witness.witness_model).ok_or_else(|| {
            schema_error(
                &root.join(QUINT_WITNESSES_PATH),
                format!(
                    "witness model {} is not a formal Quint source",
                    witness.witness_model
                ),
            )
        })?;
        require_model_module(
            witness_model,
            &witness.main_module,
            &root.join(QUINT_WITNESSES_PATH),
        )?;
        if witness.antecedent.trim().is_empty() {
            return Err(schema_error(
                &root.join(QUINT_WITNESSES_PATH),
                "witness antecedent must not be empty",
            ));
        }

        match witness.kind.as_str() {
            "safety" => {
                if liveness.contains_key(&invariant_id)
                    || witness.recorded_liveness_witness.is_some()
                    || witness.run.is_none()
                {
                    return Err(schema_error(
                        &root.join(QUINT_WITNESSES_PATH),
                        format!("safety witness {invariant_id} has invalid liveness controls"),
                    ));
                }
                let run = witness.run.as_deref().ok_or_else(|| {
                    schema_error(
                        &root.join(QUINT_WITNESSES_PATH),
                        "missing safety witness run",
                    )
                })?;
                require_identifier(run, "normal witness run", &root.join(QUINT_WITNESSES_PATH))?;
                if run.starts_with("fault") {
                    return Err(schema_error(
                        &root.join(QUINT_WITNESSES_PATH),
                        "normal witness cannot be a fault run",
                    ));
                }
                let run_body =
                    require_exact_run(witness_model, run, &root.join(QUINT_WITNESSES_PATH))?;
                if !run_body.contains(&witness.invariant) || !run_body.contains(&witness.antecedent)
                {
                    return Err(schema_error(
                        &root.join(QUINT_WITNESSES_PATH),
                        format!(
                            "normal witness {run} does not reach the recorded antecedent for {invariant_id}"
                        ),
                    ));
                }
            }
            "liveness" => {
                if witness.run.is_some() || witness.recorded_liveness_witness.is_none() {
                    return Err(schema_error(
                        &root.join(QUINT_WITNESSES_PATH),
                        format!(
                            "liveness witness {invariant_id} must reference recorded_liveness_witness only"
                        ),
                    ));
                }
                let binding = liveness.get(&invariant_id).ok_or_else(|| {
                    schema_error(
                        &root.join(QUINT_WITNESSES_PATH),
                        format!("{invariant_id} is not a recorded liveness witness"),
                    )
                })?;
                if witness.recorded_liveness_witness.as_deref() != Some(binding.name.as_str())
                    || witness.main_module != binding.main_module
                    || witness.init != binding.init
                    || witness.step != binding.step
                    || witness.max_steps != binding.max_steps
                    || witness.max_samples != binding.max_samples
                    || witness.seed != binding.seed
                {
                    return Err(schema_error(
                        &root.join(QUINT_WITNESSES_PATH),
                        format!("liveness witness {invariant_id} does not match runs.json"),
                    ));
                }
                let value = declaration_body(&witness_model.source, "val", &witness.invariant)
                    .ok_or_else(|| {
                        schema_error(
                            &root.join(QUINT_WITNESSES_PATH),
                            format!(
                                "liveness property {} is not declared in {}",
                                witness.invariant, witness.witness_model
                            ),
                        )
                    })?;
                if !value.contains(&witness.antecedent) {
                    return Err(schema_error(
                        &root.join(QUINT_WITNESSES_PATH),
                        format!(
                            "liveness antecedent is not present in {}",
                            witness.invariant
                        ),
                    ));
                }
            }
            _ => {
                return Err(schema_error(
                    &root.join(QUINT_WITNESSES_PATH),
                    format!("unknown witness kind {}", witness.kind),
                ));
            }
        }
        increment(&mut checks)?;
    }
    require_same_set(
        "normal witness controls",
        &catalog.identifiers,
        &actual_witnesses,
    )?;
    Ok(checks)
}

fn liveness_bindings(
    runs: &QuintRunsDocument,
    catalog: &BTreeSet<String>,
) -> Result<BTreeMap<String, LivenessBinding>> {
    let mut bindings = BTreeMap::new();
    for run in &runs.runs {
        let Some(witness) = &run.liveness_witness else {
            continue;
        };
        let id = format!("{}::{}", run.model, witness.name);
        if !catalog.contains(&id) {
            return Err(VerificationError::new(
                ErrorCode::SetMismatch,
                format!("runs.json liveness witness {id} is not in formal-Quint catalog"),
            ));
        }
        let binding = LivenessBinding {
            name: witness.name.clone(),
            main_module: run.main_module.clone(),
            init: run.init.clone(),
            step: run.step.clone(),
            max_steps: witness.max_steps,
            max_samples: witness.max_samples,
            seed: witness.seed.clone(),
        };
        if bindings.insert(id.clone(), binding).is_some() {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("duplicate liveness witness {id}"),
            ));
        }
    }
    if bindings.len() != EXPECTED_LIVENESS_WITNESSES {
        return Err(VerificationError::new(
            ErrorCode::SetMismatch,
            format!(
                "runs.json must declare exactly {EXPECTED_LIVENESS_WITNESSES} liveness witnesses"
            ),
        ));
    }
    Ok(bindings)
}
fn validate_simulated_catalog_coverage(
    runs: &QuintRunsDocument,
    catalog: &FormalCatalog,
    liveness: &BTreeMap<String, LivenessBinding>,
) -> Result<()> {
    let mut identifiers_by_name = BTreeMap::new();
    for identifier in &catalog.identifiers {
        let (_, name) = split_catalog_identifier(identifier, "qnt")?;
        if identifiers_by_name
            .insert(name.clone(), identifier.clone())
            .is_some()
        {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("formal-Quint catalog reuses property name {name} across authorities"),
            ));
        }
    }

    let mut simulated = BTreeSet::new();
    for run in &runs.runs {
        for invariant in &run.invariants {
            let identifier = identifiers_by_name.get(invariant).ok_or_else(|| {
                VerificationError::new(
                    ErrorCode::SetMismatch,
                    format!("runs.json invariant {invariant} is not in the formal-Quint catalog"),
                )
            })?;
            simulated.insert(identifier.clone());
        }
    }
    simulated.extend(liveness.keys().cloned());
    require_same_set(
        "simulated Quint properties",
        &catalog.identifiers,
        &simulated,
    )
}

fn validate_quint_catalog_authority(root: &Path, catalog: &FormalCatalog) -> Result<usize> {
    let models = load_quint_models(root)?;
    require_same_set(
        "formal-Quint declared files",
        &catalog.declared_files,
        &models.keys().cloned().collect(),
    )?;
    let mut checks = 0usize;
    for identifier in &catalog.identifiers {
        let (model, property) = split_catalog_identifier(identifier, "qnt")?;
        let source = models.get(&model).ok_or_else(|| {
            VerificationError::new(
                ErrorCode::SetMismatch,
                format!("formal-Quint catalog source {model} is missing"),
            )
        })?;
        require_exact_declaration(
            &source.values,
            &property,
            "Quint property",
            &root.join(MANIFEST_PATH),
        )?;
        increment(&mut checks)?;
    }
    Ok(checks)
}

fn validate_lean_catalog_authority(root: &Path, catalog: &FormalCatalog) -> Result<usize> {
    let mut checks = 0usize;
    for identifier in &catalog.identifiers {
        let (file, theorem) = split_catalog_identifier(identifier, "lean")?;
        if !catalog.declared_files.contains(&file) {
            return Err(VerificationError::new(
                ErrorCode::SetMismatch,
                format!("formal-lean catalog identifier {identifier} is outside declared_files"),
            ));
        }
        let source = read_text(&rooted_file(&root.join("formal/lean"), &file)?)?;
        if lean_theorem_count(&source, &theorem) != 1 {
            return Err(VerificationError::new(
                ErrorCode::SetMismatch,
                format!(
                    "formal-lean catalog identifier {identifier} does not occur exactly once in its authority file"
                ),
            ));
        }
        increment(&mut checks)?;
    }
    Ok(checks)
}

fn validate_redex_catalog_authority(root: &Path, catalog: &FormalCatalog) -> Result<usize> {
    let controls = catalog.redex_controls.as_ref().ok_or_else(|| {
        schema_error(
            &root.join(MANIFEST_PATH),
            "formal-redex catalog is missing its control extractor",
        )
    })?;
    let mut sources = BTreeMap::new();
    for file in &catalog.declared_files {
        let path = rooted_file(&root.join("formal/redex"), file)?;
        sources.insert(file.clone(), racket_source_code(&read_text(&path)?));
    }
    let mut checks = 0usize;
    let mut expected_controls = BTreeSet::new();
    for relation in &controls.relations {
        let (file, _) = split_catalog_identifier(relation, "rkt")?;
        if !catalog.declared_files.contains(&file) {
            return Err(VerificationError::new(
                ErrorCode::SetMismatch,
                format!("formal-redex relation {relation} is outside declared_files"),
            ));
        }
        for obligation in &controls.non_vacuity_obligations {
            expected_controls.insert(format!("control::{relation}::{obligation}"));
        }
    }
    let actual_controls = catalog
        .identifiers
        .iter()
        .filter(|identifier| identifier.starts_with("control::"))
        .cloned()
        .collect();
    require_same_set(
        "formal-redex catalog controls",
        &expected_controls,
        &actual_controls,
    )?;

    let direct_identifiers = catalog
        .identifiers
        .iter()
        .filter(|identifier| !identifier.starts_with("control::"));
    for identifier in direct_identifiers {
        let (file, name) = split_catalog_identifier(identifier, "rkt")?;
        if !catalog.declared_files.contains(&file) {
            return Err(VerificationError::new(
                ErrorCode::SetMismatch,
                format!("formal-redex catalog identifier {identifier} is outside declared_files"),
            ));
        }
        let source = sources
            .get(&file)
            .expect("declared Redex source was loaded");
        let count = if let Some(rule) = name.strip_prefix("rule::") {
            let authority = source
                .split_once("(module+ test")
                .map_or(source.as_str(), |(authority, _)| authority);
            quoted_label_count(authority, rule)
        } else {
            racket_declaration_count(source, &name)
        };
        if count != 1 {
            return Err(VerificationError::new(
                ErrorCode::SetMismatch,
                format!(
                    "formal-redex catalog identifier {identifier} does not occur exactly once in its authority file"
                ),
            ));
        }
        increment(&mut checks)?;
    }

    for control in &actual_controls {
        let relation = control.strip_prefix("control::").ok_or_else(|| {
            VerificationError::new(
                ErrorCode::Schema,
                format!("invalid formal-redex control identifier {control}"),
            )
        })?;
        let (relation, _) = relation.rsplit_once("::").ok_or_else(|| {
            VerificationError::new(
                ErrorCode::Schema,
                format!("invalid formal-redex control identifier {control}"),
            )
        })?;
        let (file, _) = split_catalog_identifier(relation, "rkt")?;
        let source = sources
            .get(&file)
            .expect("declared Redex source was loaded");
        if racket_test_case_count(source, control) != 1 {
            return Err(VerificationError::new(
                ErrorCode::SetMismatch,
                format!(
                    "formal-redex control {control} does not occur exactly once as a test-case in its authority file"
                ),
            ));
        }
        increment(&mut checks)?;
    }
    Ok(checks)
}

fn racket_test_case_count(source: &str, label: &str) -> usize {
    let needle = format!("\"{label}\"");
    source
        .match_indices(&needle)
        .filter(|(index, _)| {
            source[..*index]
                .rfind("(test-case")
                .is_some_and(|start| source[start + "(test-case".len()..*index].trim().is_empty())
        })
        .count()
}

fn racket_source_code(source: &str) -> String {
    let mut code = String::with_capacity(source.len());
    for line in source.lines() {
        let mut in_string = false;
        let mut escaped = false;
        for character in line.chars() {
            if !in_string && character == ';' {
                break;
            }
            code.push(character);
            if in_string && character == '\\' && !escaped {
                escaped = true;
                continue;
            }
            if character == '"' && !escaped {
                in_string = !in_string;
            }
            escaped = false;
        }
        code.push('\n');
    }
    code
}

fn audit_forbidden_material(root: &Path) -> Result<usize> {
    let mut sources = collect_sources(&root.join("formal/quint"), "qnt", true)?;
    sources.extend(collect_sources(&root.join("formal/redex"), "rkt", false)?);
    sources.extend(collect_sources(&root.join("formal/lean"), "lean", true)?);
    let proof = root.join("proof");
    for file in [
        "lean-assumptions.json",
        "quint-mutations.json",
        "quint-normal-witnesses.json",
    ] {
        sources.push(rooted_file(&proof, file)?);
    }
    sources.sort();

    let mut checks = 0usize;
    for source in sources {
        let text = read_text(&source)?;
        if let Some(marker) = forbidden_marker(&source, &text) {
            return Err(VerificationError::new(
                ErrorCode::Schema,
                format!(
                    "{} contains forbidden proof shortcut `{marker}`",
                    source.display()
                ),
            ));
        }
        increment(&mut checks)?;
    }
    Ok(checks)
}

fn forbidden_marker(path: &Path, text: &str) -> Option<&'static str> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if extension == "json" {
        let lower = text.to_ascii_lowercase();
        return [
            "negate-invariant",
            "expected-failure",
            "expected_failure",
            "xfail",
        ]
        .into_iter()
        .find(|marker| lower.contains(marker));
    }

    if extension == "rkt" && text.contains("#;") {
        return Some("#;");
    }
    let comment = match extension {
        "lean" => "--",
        "qnt" => "//",
        "rkt" => ";",
        _ => return None,
    };
    let code = text
        .lines()
        .map(|line| line.split_once(comment).map_or(line, |(before, _)| before))
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();
    if code.contains("expected-failure") || code.contains("expected_failure") {
        return Some("expected-failure");
    }
    if code.contains("#:skip") || code.contains("test.skip") || code.contains("skip-test") {
        return Some("skip");
    }
    for word in code.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
    {
        match word {
            "sorry" => return Some("sorry"),
            "admit" => return Some("admit"),
            "axiom" => return Some("axiom"),
            "implemented_by" => return Some("implemented_by"),
            "xfail" => return Some("xfail"),
            _ => {}
        }
    }
    None
}

fn validate_lean_assumptions(
    root: &Path,
    catalog: &FormalCatalog,
    assumptions: &LeanAssumptions,
) -> Result<Vec<LeanTheorem>> {
    let path = root.join(LEAN_ASSUMPTIONS_PATH);
    if assumptions.schema != "bamti.lean-assumptions/v1" || assumptions.lean_version != "4.32.1" {
        return Err(schema_error(
            &path,
            "unexpected Lean assumptions schema or Lean version",
        ));
    }
    if !is_lower_hex(&assumptions.source_sha256, 64) {
        return Err(schema_error(
            &path,
            "source_sha256 must be lowercase SHA-256",
        ));
    }
    if assumptions.forbidden_constructs.admit != 0
        || assumptions.forbidden_constructs.axiom != 0
        || assumptions.forbidden_constructs.implemented_by != 0
        || assumptions.forbidden_constructs.sorry != 0
        || !assumptions.user_axioms.is_empty()
    {
        return Err(schema_error(
            &path,
            "Lean assumptions manifest records an unexpected proof hole or user axiom",
        ));
    }
    let allowed = string_set(&STANDARD_LEAN_ASSUMPTIONS);
    let observed: BTreeSet<_> = assumptions
        .observed_kernel_assumptions
        .iter()
        .cloned()
        .collect();
    if observed.len() != assumptions.observed_kernel_assumptions.len() || observed != allowed {
        return Err(schema_error(
            &path,
            "observed_kernel_assumptions must be the exact standard Lean set",
        ));
    }

    let actual_digest = lean_source_digest(root, &catalog.declared_files)?;
    if actual_digest != assumptions.source_sha256 {
        return Err(VerificationError::new(
            ErrorCode::Digest,
            format!(
                "{} source digest mismatch: manifest={}, actual={actual_digest}",
                path.display(),
                assumptions.source_sha256
            ),
        ));
    }

    let expected = catalog_identifier_pairs(&catalog.identifiers, "lean")?;
    let mut actual = BTreeMap::new();
    let mut theorems = Vec::with_capacity(assumptions.public_theorems.len());
    for theorem in &assumptions.public_theorems {
        validate_relative_path(&theorem.file, "lean theorem file", &path)?;
        let short = theorem
            .name
            .rsplit_once('.')
            .map(|(_, short)| short)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                schema_error(
                    &path,
                    format!("Lean theorem name {} is not qualified", theorem.name),
                )
            })?;
        require_identifier(short, "Lean theorem", &path)?;
        let key = (theorem.file.clone(), short.to_owned());
        if actual.insert(key.clone(), theorem.name.clone()).is_some() {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("duplicate public Lean theorem {}::{}", key.0, key.1),
            ));
        }
        let source = read_text(&rooted_file(&root.join("formal/lean"), &theorem.file)?)?;
        if lean_theorem_count(&source, short) != 1 {
            return Err(schema_error(
                &path,
                format!(
                    "public theorem {} does not occur exactly once in {}",
                    theorem.name, theorem.file
                ),
            ));
        }
        theorems.push(LeanTheorem {
            full_name: theorem.name.clone(),
        });
    }
    let actual_set: BTreeSet<_> = actual.into_keys().collect();
    require_same_set_pairs("public Lean theorems", &expected, &actual_set)?;
    Ok(theorems)
}

fn lean_source_digest(root: &Path, declared_files: &BTreeSet<String>) -> Result<String> {
    if declared_files.is_empty() {
        return Err(schema_error(
            root,
            "formal-lean catalog has no declared files",
        ));
    }
    let base = root.join("formal/lean");
    let mut files = BTreeSet::new();
    for path in collect_sources(&base, "lean", true)? {
        let relative = path_to_utf8(
            path.strip_prefix(&base).map_err(|error| {
                VerificationError::new(
                    ErrorCode::Io,
                    format!("cannot relativize {}: {error}", path.display()),
                )
            })?,
            "formal-lean source path",
        )?;
        files.insert(relative);
    }
    for file in declared_files {
        if !files.contains(file) {
            return Err(schema_error(
                root,
                format!("formal-lean declared file {file} is missing"),
            ));
        }
    }
    let mut digest = Sha256::new();
    for file in files {
        let path = rooted_file(&base, &file)?;
        digest.update(file.as_bytes());
        digest.update([0]);
        digest.update(read_bytes(&path)?);
        digest.update([0]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn parse_lean_axioms(
    output: &Output,
    theorems: &[LeanTheorem],
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let expected: BTreeSet<_> = theorems
        .iter()
        .map(|theorem| theorem.full_name.clone())
        .collect();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut observed = BTreeMap::new();
    for line in text.lines() {
        if let Some((left, _)) = line.split_once("does not depend on any axioms") {
            let name = left
                .trim()
                .trim_matches(|character| character == '\'' || character == '`')
                .trim();
            if expected.contains(name)
                && observed.insert(name.to_owned(), BTreeSet::new()).is_some()
            {
                return Err(VerificationError::new(
                    ErrorCode::Duplicate,
                    format!("Lean axiom audit emitted duplicate result for {name}"),
                ));
            }
            continue;
        }
        let Some((left, right)) = line.split_once("depends on axioms:") else {
            continue;
        };
        let name = left
            .trim()
            .trim_matches(|character| character == '\'' || character == '`')
            .trim();
        if !expected.contains(name) {
            continue;
        }
        let list = right.trim();
        let list = list
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .ok_or_else(|| {
                VerificationError::new(
                    ErrorCode::Schema,
                    format!("cannot parse Lean axiom output for {name}"),
                )
            })?;
        let axioms = if list.trim().is_empty() {
            BTreeSet::new()
        } else {
            list.split(',')
                .map(|value| value.trim().to_owned())
                .collect()
        };
        if observed.insert(name.to_owned(), axioms).is_some() {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("Lean axiom audit emitted duplicate result for {name}"),
            ));
        }
    }
    let actual: BTreeSet<_> = observed.keys().cloned().collect();
    require_same_set("Lean axiom audit output", &expected, &actual)?;
    Ok(observed)
}

fn validate_quint_runs(
    root: &Path,
    manifest: &QuintRunsDocument,
    models: &BTreeMap<String, QuintModel>,
) -> Result<()> {
    let path = root.join(QUINT_RUNS_PATH);
    if manifest.schema != "bamti.quint-runs/v1"
        || manifest.generator_backend != "quint"
        || manifest.node_version != "24.18.0"
        || manifest.tool.name != "@informalsystems/quint"
        || manifest.tool.version != QUINT_VERSION
        || manifest.tool.archive_digest_algorithm != "sha512"
        || manifest.tool.archive_digest.trim().is_empty()
        || !is_lower_hex(&manifest.package_lock_sha256, 64)
    {
        return Err(schema_error(
            &path,
            "runs.json does not describe the pinned Quint 0.32.0 runner",
        ));
    }
    if manifest.runs.len() != 6 {
        return Err(schema_error(
            &path,
            "runs.json must contain exactly six bounded safety simulations",
        ));
    }
    let mut seen_models = BTreeSet::new();
    let mut seen_invariants = BTreeSet::new();
    for run in &manifest.runs {
        validate_relative_path(&run.model, "Quint model", &path)?;
        if Path::new(&run.model)
            .extension()
            .and_then(|value| value.to_str())
            != Some("qnt")
        {
            return Err(schema_error(
                &path,
                format!("{} is not a Quint model", run.model),
            ));
        }
        let source = models
            .get(&run.model)
            .ok_or_else(|| schema_error(&path, format!("missing Quint model {}", run.model)))?;
        require_model_module(source, &run.main_module, &path)?;
        validate_simulation_controls(
            &run.main_module,
            &run.init,
            &run.step,
            "rust",
            1,
            run.max_steps,
            run.max_samples,
            &run.seed,
            &path,
        )?;
        if !seen_models.insert(run.model.clone()) {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("duplicate Quint run model {}", run.model),
            ));
        }
        if run.invariants.is_empty() {
            return Err(schema_error(
                &path,
                format!("Quint run {} has no named invariants", run.model),
            ));
        }
        for invariant in &run.invariants {
            require_identifier(invariant, "Quint invariant", &path)?;
            let id = format!("{}::{invariant}", run.model);
            if !seen_invariants.insert(id.clone()) {
                return Err(VerificationError::new(
                    ErrorCode::Duplicate,
                    format!("duplicate Quint invariant {id}"),
                ));
            }
        }
        validate_relative_path(&run.trace, "Quint trace", &path)?;
        if !is_lower_hex(&run.trace_sha256, 64) {
            return Err(schema_error(
                &path,
                format!("Quint trace digest for {} is invalid", run.model),
            ));
        }
        let trace = rooted_file(root, &run.trace)?;
        if !trace.is_file() {
            return Err(schema_error(
                &path,
                format!("missing Quint trace {}", trace.display()),
            ));
        }
        if let Some(witness) = &run.liveness_witness {
            require_identifier(&witness.name, "liveness witness", &path)?;
            if witness.max_steps == 0
                || witness.max_samples == 0
                || witness.witnessed == 0
                || witness.witnessed > witness.max_samples
                || !decimal_seed(&witness.seed)
            {
                return Err(schema_error(
                    &path,
                    format!("invalid liveness witness {}", witness.name),
                ));
            }
        }
    }
    Ok(())
}

fn validate_simulation_controls(
    main_module: &str,
    init: &str,
    step: &str,
    backend: &str,
    threads: u64,
    max_steps: u64,
    max_samples: u64,
    seed: &str,
    path: &Path,
) -> Result<()> {
    require_identifier(main_module, "main module", path)?;
    if init != "init"
        || step != "step"
        || backend != "rust"
        || threads != 1
        || max_steps == 0
        || max_samples == 0
        || !decimal_seed(seed)
    {
        return Err(schema_error(
            path,
            "simulation controls must use default init/step, Rust, one thread, positive bounds, and a decimal seed",
        ));
    }
    Ok(())
}

fn read_quint_runs(root: &Path) -> Result<QuintRunsDocument> {
    read_strict_json(&root.join(QUINT_RUNS_PATH))
}

fn read_mutations(root: &Path) -> Result<MutationDocument> {
    read_strict_json(&root.join(QUINT_MUTATIONS_PATH))
}

fn read_normal_witnesses(root: &Path) -> Result<NormalWitnessDocument> {
    read_strict_json(&root.join(QUINT_WITNESSES_PATH))
}

fn read_lean_assumptions(root: &Path) -> Result<LeanAssumptions> {
    read_strict_json(&root.join(LEAN_ASSUMPTIONS_PATH))
}

fn formal_catalog(root: &Path, wanted: &str) -> Result<FormalCatalog> {
    let path = root.join(MANIFEST_PATH);
    let value = parse_strict_json(&path)?;
    let object = json_object(&value, &path, "verification manifest")?;
    let catalogs = json_array(
        required_json(object, "catalogs", &path)?,
        &path,
        "verification manifest catalogs",
    )?;
    let mut found = None;
    for catalog in catalogs {
        let catalog_object = json_object(catalog, &path, "catalog")?;
        let id = json_string(
            required_json(catalog_object, "id", &path)?,
            &path,
            "catalog id",
        )?;
        if id != wanted {
            continue;
        }
        if found.is_some() {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("duplicate catalog {wanted}"),
            ));
        }
        let identifier_count = json_u64(
            required_json(catalog_object, "identifier_count", &path)?,
            &path,
            "catalog identifier_count",
        )?;
        let identifiers = json_array(
            required_json(catalog_object, "identifiers", &path)?,
            &path,
            "catalog identifiers",
        )?
        .iter()
        .map(|value| json_string(value, &path, "catalog identifier"))
        .collect::<Result<Vec<_>>>()?;
        let actual: BTreeSet<_> = identifiers.iter().cloned().collect();
        if actual.len() != identifiers.len() {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("catalog {wanted} has duplicate identifiers"),
            ));
        }
        if usize::try_from(identifier_count).ok() != Some(identifiers.len()) {
            return Err(schema_error(
                &path,
                format!("catalog {wanted} identifier_count does not match identifiers"),
            ));
        }
        let extractor = json_object(
            required_json(catalog_object, "extractor", &path)?,
            &path,
            "catalog extractor",
        )?;
        let files = json_array(
            required_json(extractor, "declared_files", &path)?,
            &path,
            "catalog declared_files",
        )?
        .iter()
        .map(|value| json_string(value, &path, "catalog declared file"))
        .collect::<Result<Vec<_>>>()?;
        let declared_files: BTreeSet<_> = files.iter().cloned().collect();
        if declared_files.len() != files.len() {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("catalog {wanted} has duplicate declared files"),
            ));
        }
        for file in &declared_files {
            validate_relative_path(file, "catalog declared file", &path)?;
        }
        let redex_controls = if wanted == "formal-redex" {
            Some(parse_redex_controls(extractor, &path)?)
        } else {
            None
        };
        found = Some(FormalCatalog {
            identifiers: actual,
            declared_files,
            redex_controls,
        });
    }
    found.ok_or_else(|| schema_error(&path, format!("missing catalog {wanted}")))
}

fn parse_redex_controls(
    extractor: &serde_json::Map<String, Value>,
    path: &Path,
) -> Result<RedexControls> {
    let control_rule = json_string(
        required_json(extractor, "control_rule", path)?,
        path,
        "formal-redex control_rule",
    )?;
    if control_rule != "relations x non_vacuity_obligations" {
        return Err(schema_error(
            path,
            "formal-redex control_rule must be `relations x non_vacuity_obligations`",
        ));
    }
    let relations = json_array(
        required_json(extractor, "relations", path)?,
        path,
        "formal-redex relations",
    )?
    .iter()
    .map(|value| json_string(value, path, "formal-redex relation"))
    .collect::<Result<Vec<_>>>()?;
    let non_vacuity_obligations = json_array(
        required_json(extractor, "non_vacuity_obligations", path)?,
        path,
        "formal-redex non_vacuity_obligations",
    )?
    .iter()
    .map(|value| json_string(value, path, "formal-redex non_vacuity_obligation"))
    .collect::<Result<Vec<_>>>()?;
    let relation_count = relations.len();
    let obligation_count = non_vacuity_obligations.len();
    let relations: BTreeSet<_> = relations.into_iter().collect();
    let non_vacuity_obligations: BTreeSet<_> = non_vacuity_obligations.into_iter().collect();
    if relations.len() != relation_count
        || non_vacuity_obligations.len() != obligation_count
        || relations.is_empty()
        || non_vacuity_obligations.is_empty()
        || relations
            .iter()
            .any(|relation| split_catalog_identifier(relation, "rkt").is_err())
        || non_vacuity_obligations.iter().any(|obligation| {
            obligation.is_empty()
                || obligation.contains("::")
                || !obligation
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        })
    {
        return Err(schema_error(path, "formal-redex controls are malformed"));
    }
    Ok(RedexControls {
        relations,
        non_vacuity_obligations,
    })
}

fn split_catalog_identifier(identifier: &str, extension: &str) -> Result<(String, String)> {
    let (file, name) = identifier.split_once("::").ok_or_else(|| {
        VerificationError::new(
            ErrorCode::Schema,
            format!("invalid formal catalog identifier {identifier}"),
        )
    })?;
    if Path::new(file).extension().and_then(|value| value.to_str()) != Some(extension)
        || name.is_empty()
    {
        return Err(VerificationError::new(
            ErrorCode::Schema,
            format!("formal catalog identifier {identifier} has the wrong authority kind"),
        ));
    }
    Ok((file.to_owned(), name.to_owned()))
}

fn catalog_identifier_pairs(
    identifiers: &BTreeSet<String>,
    extension: &str,
) -> Result<BTreeSet<(String, String)>> {
    identifiers
        .iter()
        .map(|identifier| split_catalog_identifier(identifier, extension))
        .collect()
}

fn catalog_id(model: &str, invariant: &str, path: &Path) -> Result<String> {
    validate_relative_path(model, "Quint model", path)?;
    require_identifier(invariant, "Quint invariant", path)?;
    Ok(format!("{model}::{invariant}"))
}

fn load_quint_models(root: &Path) -> Result<BTreeMap<String, QuintModel>> {
    let base = root.join("formal/quint");
    let mut models = BTreeMap::new();
    for path in collect_sources(&base, "qnt", true)? {
        let relative = path.strip_prefix(&base).map_err(|error| {
            VerificationError::new(
                ErrorCode::Io,
                format!("cannot relativize {}: {error}", path.display()),
            )
        })?;
        let relative = path_to_utf8(relative, "Quint model path")?;
        let source = read_text(&path)?;
        let model = parse_quint_model(&source);
        if models.insert(relative.clone(), model).is_some() {
            return Err(VerificationError::new(
                ErrorCode::Duplicate,
                format!("duplicate Quint model {relative}"),
            ));
        }
    }
    if models.is_empty() {
        return Err(schema_error(&base, "formal/quint contains no models"));
    }
    Ok(models)
}

fn parse_quint_model(source: &str) -> QuintModel {
    let mut modules = BTreeMap::new();
    let mut values = BTreeMap::new();
    let mut actions = BTreeMap::new();
    let mut runs = BTreeMap::new();
    for line in source.lines() {
        let code = line
            .split_once("//")
            .map_or(line, |(before, _)| before)
            .trim();
        for (keyword, declarations) in [
            ("module", &mut modules),
            ("val", &mut values),
            ("action", &mut actions),
            ("run", &mut runs),
        ] {
            if let Some(name) = declaration_name(code, keyword) {
                *declarations.entry(name.to_owned()).or_insert(0usize) += 1;
            }
        }
    }
    QuintModel {
        source: source.to_owned(),
        modules,
        values,
        actions,
        runs,
    }
}

fn declaration_name<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let remainder = line
        .strip_prefix(keyword)?
        .strip_prefix(char::is_whitespace)?;
    let name = remainder
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .next()?;
    (!name.is_empty()).then_some(name)
}

fn declaration_body<'a>(source: &'a str, keyword: &str, wanted: &str) -> Option<&'a str> {
    let mut start = None;
    let mut offset = 0usize;
    for line in source.split_inclusive('\n') {
        let code = line
            .split_once("//")
            .map_or(line, |(before, _)| before)
            .trim();
        if let Some(name) = declaration_name(code, keyword) {
            if name == wanted {
                start = Some(offset);
            } else if let Some(beginning) = start {
                return source.get(beginning..offset);
            }
        } else if start.is_some()
            && ["module", "val", "action", "run"]
                .iter()
                .any(|candidate| declaration_name(code, candidate).is_some())
        {
            return source.get(start?..offset);
        }
        offset = offset.saturating_add(line.len());
    }
    start.and_then(|beginning| source.get(beginning..))
}

fn require_exact_declaration(
    declarations: &BTreeMap<String, usize>,
    name: &str,
    kind: &str,
    path: &Path,
) -> Result<()> {
    match declarations.get(name).copied() {
        Some(1) => Ok(()),
        Some(count) => Err(schema_error(
            path,
            format!("{kind} {name} occurs {count} times"),
        )),
        None => Err(schema_error(path, format!("missing {kind} {name}"))),
    }
}

fn require_exact_run<'a>(model: &'a QuintModel, run: &str, path: &Path) -> Result<&'a str> {
    require_exact_declaration(&model.runs, run, "Quint run", path)?;
    declaration_body(&model.source, "run", run)
        .ok_or_else(|| schema_error(path, format!("cannot read Quint run {run}")))
}

fn require_model_module(model: &QuintModel, main_module: &str, path: &Path) -> Result<()> {
    require_exact_declaration(&model.modules, main_module, "Quint main module", path)
}

fn rooted_qnt_path(root: &Path, model: &str) -> Result<PathBuf> {
    validate_relative_path(model, "Quint model", root)?;
    rooted_file(&root.join("formal/quint"), model)
}

fn lean_theorem_count(source: &str, wanted: &str) -> usize {
    source
        .lines()
        .filter_map(|line| {
            let code = line
                .split_once("--")
                .map_or(line, |(before, _)| before)
                .trim();
            let remainder = code
                .strip_prefix("theorem ")
                .or_else(|| code.strip_prefix("private theorem "))?;
            remainder
                .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .next()
        })
        .filter(|name| *name == wanted)
        .count()
}

fn racket_declaration_count(source: &str, wanted: &str) -> usize {
    let mut count = 0usize;
    let mut pending = None::<&str>;
    for line in source.lines() {
        let code = line.trim();
        if let Some(kind) = pending {
            if code.is_empty() {
                continue;
            }
            if kind == "metafunction" {
                let name = code
                    .split(|character: char| character == ':' || character.is_whitespace())
                    .next()
                    .unwrap_or_default();
                if name == wanted {
                    count = count.saturating_add(1);
                }
            } else if let Some(open) = code.find('(') {
                let name = code[open + 1..]
                    .split(|character: char| {
                        !(character.is_ascii_alphanumeric() || "~>?!-".contains(character))
                    })
                    .next()
                    .unwrap_or_default();
                if name == wanted {
                    count = count.saturating_add(1);
                }
            }
            pending = None;
            continue;
        }
        if let Some(rest) = code.strip_prefix("(define ") {
            let name = rest
                .trim_start_matches('(')
                .split(|character: char| {
                    !(character.is_ascii_alphanumeric() || "~>?!-".contains(character))
                })
                .next()
                .unwrap_or_default();
            if name == wanted {
                count = count.saturating_add(1);
            }
        } else if code.starts_with("(define-metafunction") {
            pending = Some("metafunction");
        } else if code.starts_with("(define-judgment-form") {
            pending = Some("judgment");
        }
    }
    count
}

fn quoted_label_count(source: &str, wanted: &str) -> usize {
    let needle = format!("\"{wanted}\"");
    source.matches(&needle).count()
}

fn quint_launcher(root: &Path, overrides: &ToolOverrides) -> Result<PathBuf> {
    if let Some(path) = &overrides.quint {
        return require_tool_file(path, "Quint launcher");
    }
    let launcher = root
        .join("formal/quint/node_modules/.bin")
        .join(if cfg!(windows) { "quint.cmd" } else { "quint" });
    require_tool_file(&launcher, "repository-local Quint launcher")?;
    let package = root.join("formal/quint/node_modules/@informalsystems/quint/package.json");
    let value = parse_strict_json(&package)?;
    let object = json_object(&value, &package, "installed Quint package")?;
    if json_string(
        required_json(object, "version", &package)?,
        &package,
        "installed Quint version",
    )? != QUINT_VERSION
    {
        return Err(VerificationError::new(
            ErrorCode::ToolMissing,
            "installed Quint package is not version 0.32.0",
        ));
    }
    Ok(launcher)
}

fn racket_tools(overrides: &ToolOverrides) -> Result<(PathBuf, PathBuf)> {
    let raco = match &overrides.raco {
        Some(path) => require_tool_file(path, "raco")?,
        None => find_path_tool("raco")?,
    };
    let racket = match &overrides.racket {
        Some(path) => require_tool_file(path, "racket")?,
        None => {
            let directory = raco.parent().ok_or_else(|| {
                VerificationError::new(ErrorCode::ToolMissing, "raco has no executable directory")
            })?;
            require_tool_file(
                &directory.join(if cfg!(windows) {
                    "racket.exe"
                } else {
                    "racket"
                }),
                "racket sibling of raco",
            )?
        }
    };
    Ok((raco, racket))
}

fn lake_tool(overrides: &ToolOverrides) -> Result<PathBuf> {
    match &overrides.lake {
        Some(path) => require_tool_file(path, "lake"),
        None => find_path_tool("lake"),
    }
}

fn verify_racket_pin(root: &Path, racket: &Path) -> Result<()> {
    let output = run_tool(root, racket, &["--version".into()], "racket --version")?;
    let reported = String::from_utf8_lossy(&output.stdout);
    let output = reported.trim().trim_end_matches('.').trim();
    let normalized = output.strip_prefix("Welcome to ").unwrap_or(output);
    if normalized != RACKET_VERSION {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!(
                "racket sibling {} reported `{normalized}`, expected `{RACKET_VERSION}`",
                racket.display()
            ),
        ));
    }
    Ok(())
}

fn find_path_tool(name: &str) -> Result<PathBuf> {
    let path = env::var_os("PATH").ok_or_else(|| {
        VerificationError::new(
            ErrorCode::ToolMissing,
            format!("PATH is not set while resolving {name}"),
        )
    })?;
    for directory in env::split_paths(&path) {
        let candidate = directory.join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_owned()
        });
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(VerificationError::new(
        ErrorCode::ToolMissing,
        format!("cannot resolve {name} through PATH"),
    ))
}

fn require_tool_file(path: &Path, name: &str) -> Result<PathBuf> {
    if path.is_file() {
        Ok(path.to_path_buf())
    } else {
        Err(VerificationError::new(
            ErrorCode::ToolMissing,
            format!("missing {name}: {}", path.display()),
        ))
    }
}

fn run_tool(
    cwd: &Path,
    tool: &Path,
    arguments: &[std::ffi::OsString],
    label: &str,
) -> Result<Output> {
    let output = Command::new(tool)
        .args(arguments)
        .current_dir(cwd)
        .output()
        .map_err(|error| {
            let code = if error.kind() == ErrorKind::NotFound {
                ErrorCode::ToolMissing
            } else {
                ErrorCode::ToolFailed
            };
            VerificationError::new(code, format!("cannot run {label}: {error}"))
        })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!(
                "{label} exited {}: {}",
                output.status,
                bounded_child_output(&output)
            ),
        ))
    }
}

fn validate_quint_liveness_output(output: &Output, witness: &LivenessWitness) -> Result<()> {
    let stdout = std::str::from_utf8(&output.stdout).map_err(|error| {
        VerificationError::new(
            ErrorCode::ToolFailed,
            format!("Quint liveness witness output is not UTF-8: {error}"),
        )
    })?;
    let lines = stdout
        .lines()
        .filter(|line| line.contains(" was witnessed"))
        .map(parse_quint_witness_line)
        .collect::<Result<Vec<_>>>()?;
    if lines.len() != 1 {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!(
                "Quint liveness witness output must contain exactly one witness line, found {}",
                lines.len()
            ),
        ));
    }
    let observed = &lines[0];
    if observed.name != witness.name
        || observed.witnessed == 0
        || observed.witnessed != witness.witnessed
        || observed.explored != witness.max_samples
    {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            format!(
                "Quint liveness witness output does not match runs.json for {}",
                witness.name
            ),
        ));
    }
    Ok(())
}

fn parse_quint_witness_line(line: &str) -> Result<ObservedWitness> {
    let (name, reported) = line.split_once(" was witnessed in ").ok_or_else(|| {
        VerificationError::new(
            ErrorCode::ToolFailed,
            "malformed Quint liveness witness line",
        )
    })?;
    let (witnessed, reported) = reported.split_once(" trace(s) out of ").ok_or_else(|| {
        VerificationError::new(
            ErrorCode::ToolFailed,
            "malformed Quint liveness witness line",
        )
    })?;
    let (explored, percentage) = reported.split_once(" explored (").ok_or_else(|| {
        VerificationError::new(
            ErrorCode::ToolFailed,
            "malformed Quint liveness witness line",
        )
    })?;
    let percentage = percentage.strip_suffix(')').ok_or_else(|| {
        VerificationError::new(
            ErrorCode::ToolFailed,
            "malformed Quint liveness witness line",
        )
    })?;
    let witnessed = parse_quint_count(witnessed)?;
    let explored = parse_quint_count(explored)?;
    if name.is_empty()
        || explored == 0
        || witnessed > explored
        || percentage != quint_witness_percentage(witnessed, explored)?
    {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            "malformed Quint liveness witness line",
        ));
    }
    Ok(ObservedWitness {
        name: name.to_owned(),
        witnessed,
        explored,
    })
}

fn parse_quint_count(value: &str) -> Result<u64> {
    let count = value.parse::<u64>().map_err(|_| {
        VerificationError::new(
            ErrorCode::ToolFailed,
            "malformed Quint liveness witness count",
        )
    })?;
    if count.to_string() != value {
        return Err(VerificationError::new(
            ErrorCode::ToolFailed,
            "malformed Quint liveness witness count",
        ));
    }
    Ok(count)
}

fn quint_witness_percentage(witnessed: u64, explored: u64) -> Result<String> {
    let hundredths = witnessed
        .checked_mul(10_000)
        .and_then(|value| value.checked_add(explored / 2))
        .ok_or_else(|| {
            VerificationError::new(
                ErrorCode::ToolFailed,
                "Quint liveness witness percentage overflow",
            )
        })?
        / explored;
    Ok(format!("{}.{:02}%", hundredths / 100, hundredths % 100))
}

fn bounded_child_output(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let combined = match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "child exited without output".to_owned(),
        (false, true) => stdout,
        (true, false) => stderr,
        (false, false) => format!("stdout: {stdout}; stderr: {stderr}"),
    };
    if combined.len() > OUTPUT_PREVIEW_LIMIT {
        let mut limit = OUTPUT_PREVIEW_LIMIT;
        while limit > 0 && !combined.is_char_boundary(limit) {
            limit -= 1;
        }
        let mut text = combined[..limit].trim_end().to_owned();
        text.push_str(" …[truncated]");
        text
    } else {
        combined
    }
}

fn collect_sources(root: &Path, extension: &str, skip_node_modules: bool) -> Result<Vec<PathBuf>> {
    let mut pending = vec![root.to_path_buf()];
    let mut sources = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(|error| io_error(&directory, error))?;
        for entry in entries {
            let entry = entry.map_err(|error| io_error(&directory, error))?;
            let path = entry.path();
            let kind = entry.file_type().map_err(|error| io_error(&path, error))?;
            if kind.is_symlink() {
                return Err(schema_error(
                    &path,
                    "formal audit does not follow symbolic links",
                ));
            }
            if kind.is_dir() {
                if skip_node_modules
                    && (entry.file_name() == "node_modules" || entry.file_name() == ".lake")
                {
                    continue;
                }
                pending.push(path);
            } else if kind.is_file()
                && path.extension().and_then(|value| value.to_str()) == Some(extension)
            {
                sources.push(path);
            }
        }
    }
    sources.sort();
    Ok(sources)
}

fn rooted_directory(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = rooted_path(root, relative)?;
    if path.is_dir() {
        Ok(path)
    } else {
        Err(schema_error(&path, "required directory is missing"))
    }
}

fn rooted_file(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = rooted_path(root, relative)?;
    let metadata = fs::symlink_metadata(&path).map_err(|error| io_error(&path, error))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(schema_error(&path, "required regular file is missing"));
    }
    Ok(path)
}

fn rooted_path(root: &Path, relative: &str) -> Result<PathBuf> {
    validate_relative_path(relative, "repository path", root)?;
    Ok(root.join(relative))
}

fn validate_relative_path(value: &str, kind: &str, context: &Path) -> Result<()> {
    if value.is_empty() {
        return Err(schema_error(context, format!("{kind} must not be empty")));
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(schema_error(
            context,
            format!("{kind} must be a normalized relative path"),
        ));
    }
    Ok(())
}

fn root_relative(root: &Path, path: &Path) -> Result<PathBuf> {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|error| {
            VerificationError::new(
                ErrorCode::Io,
                format!(
                    "{} is outside root {}: {error}",
                    path.display(),
                    root.display()
                ),
            )
        })
}

fn path_to_utf8(path: &Path, kind: &str) -> Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        VerificationError::new(
            ErrorCode::Schema,
            format!("{kind} is not valid UTF-8: {}", path.display()),
        )
    })
}

fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|error| io_error(path, error))
}

fn read_text(path: &Path) -> Result<String> {
    let bytes = read_bytes(path)?;
    String::from_utf8(bytes)
        .map_err(|error| schema_error(path, format!("file is not UTF-8: {error}")))
}

fn io_error(path: &Path, error: std::io::Error) -> VerificationError {
    VerificationError::new(ErrorCode::Io, format!("{}: {error}", path.display()))
}

fn schema_error(path: &Path, detail: impl Into<String>) -> VerificationError {
    VerificationError::new(
        ErrorCode::Schema,
        format!("{}: {}", path.display(), detail.into()),
    )
}

fn increment(value: &mut usize) -> Result<()> {
    *value = value
        .checked_add(1)
        .ok_or_else(|| VerificationError::new(ErrorCode::Schema, "check count overflow"))?;
    Ok(())
}

fn checked_add(left: usize, right: usize) -> Result<usize> {
    left.checked_add(right)
        .ok_or_else(|| VerificationError::new(ErrorCode::Schema, "check count overflow"))
}

fn decimal_seed(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn require_identifier(value: &str, kind: &str, path: &Path) -> Result<()> {
    let mut characters = value.bytes();
    let Some(first) = characters.next() else {
        return Err(schema_error(path, format!("{kind} must not be empty")));
    };
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !characters.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(schema_error(
            path,
            format!("{kind} `{value}` is not an identifier"),
        ));
    }
    Ok(())
}

fn string_set(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn require_same_set(
    kind: &str,
    expected: &BTreeSet<String>,
    actual: &BTreeSet<String>,
) -> Result<()> {
    if expected == actual {
        return Ok(());
    }
    Err(VerificationError::new(
        ErrorCode::SetMismatch,
        format!(
            "{kind} differ: missing [{}]; unexpected [{}]",
            set_preview(expected.difference(actual)),
            set_preview(actual.difference(expected))
        ),
    ))
}

fn require_same_set_pairs(
    kind: &str,
    expected: &BTreeSet<(String, String)>,
    actual: &BTreeSet<(String, String)>,
) -> Result<()> {
    if expected == actual {
        return Ok(());
    }
    let format_pairs = |values: &BTreeSet<(String, String)>| {
        values
            .iter()
            .take(8)
            .map(|(file, name)| format!("{file}::{name}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let missing: BTreeSet<_> = expected.difference(actual).cloned().collect();
    let extra: BTreeSet<_> = actual.difference(expected).cloned().collect();
    Err(VerificationError::new(
        ErrorCode::SetMismatch,
        format!(
            "{kind} differ: missing [{}]; unexpected [{}]",
            format_pairs(&missing),
            format_pairs(&extra)
        ),
    ))
}

fn set_preview<'a>(values: impl Iterator<Item = &'a String>) -> String {
    values.take(8).cloned().collect::<Vec<_>>().join(", ")
}

fn validate_control_tool(tool: &ControlTool, path: &str) -> Result<()> {
    if tool.name == "@informalsystems/quint" && tool.version == QUINT_VERSION {
        Ok(())
    } else {
        Err(schema_error(
            Path::new(path),
            "control manifest does not pin Quint 0.32.0",
        ))
    }
}

fn parse_strict_json(path: &Path) -> Result<Value> {
    let text = read_text(path)?;
    let mut deserializer = serde_json::Deserializer::from_str(&text);
    let value = StrictJson::deserialize(&mut deserializer).map_err(|error| {
        let code = if error.to_string().contains("duplicate JSON key") {
            ErrorCode::Duplicate
        } else {
            ErrorCode::Json
        };
        VerificationError::new(code, format!("{}: {error}", path.display()))
    })?;
    deserializer.end().map_err(|error| {
        VerificationError::new(ErrorCode::Json, format!("{}: {error}", path.display()))
    })?;
    Ok(value.0)
}

fn read_strict_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    serde_json::from_value(parse_strict_json(path)?)
        .map_err(|error| schema_error(path, error.to_string()))
}

fn json_object<'a>(
    value: &'a Value,
    path: &Path,
    context: &str,
) -> Result<&'a serde_json::Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| schema_error(path, format!("{context} must be an object")))
}

fn json_array<'a>(value: &'a Value, path: &Path, context: &str) -> Result<&'a [Value]> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| schema_error(path, format!("{context} must be an array")))
}

fn json_string(value: &Value, path: &Path, context: &str) -> Result<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| schema_error(path, format!("{context} must be a string")))
}

fn json_u64(value: &Value, path: &Path, context: &str) -> Result<u64> {
    value
        .as_u64()
        .ok_or_else(|| schema_error(path, format!("{context} must be an unsigned integer")))
}

fn required_json<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<&'a Value> {
    object
        .get(key)
        .ok_or_else(|| schema_error(path, format!("missing required key `{key}`")))
}

struct StrictJson(Value);

impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StrictVisitor;

        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictJson;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value without duplicate object keys")
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJson(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJson(Value::Number(value.into())))
            }

            fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJson(Value::Number(value.into())))
            }

            fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(Value::Number)
                    .map(StrictJson)
                    .ok_or_else(|| E::custom("JSON number is not finite"))
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJson(Value::String(value.to_owned())))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJson(Value::String(value)))
            }

            fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJson(Value::Null))
            }

            fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictJson(Value::Null))
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<StrictJson>()? {
                    values.push(value.0);
                }
                Ok(StrictJson(Value::Array(values)))
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON key `{key}`"
                        )));
                    }
                    let value = map.next_value::<StrictJson>()?;
                    values.insert(key, value.0);
                }
                Ok(StrictJson(Value::Object(values)))
            }
        }

        deserializer.deserialize_any(StrictVisitor)
    }
}

#[derive(Default)]
struct ToolOverrides {
    quint: Option<PathBuf>,
    raco: Option<PathBuf>,
    racket: Option<PathBuf>,
    lake: Option<PathBuf>,
}

struct FormalCatalog {
    identifiers: BTreeSet<String>,
    declared_files: BTreeSet<String>,
    redex_controls: Option<RedexControls>,
}

struct RedexControls {
    relations: BTreeSet<String>,
    non_vacuity_obligations: BTreeSet<String>,
}

struct ObservedWitness {
    name: String,
    witnessed: u64,
    explored: u64,
}

struct QuintModel {
    source: String,
    modules: BTreeMap<String, usize>,
    values: BTreeMap<String, usize>,
    actions: BTreeMap<String, usize>,
    runs: BTreeMap<String, usize>,
}

struct LivenessBinding {
    name: String,
    main_module: String,
    init: String,
    step: String,
    max_steps: u64,
    max_samples: u64,
    seed: String,
}

struct LeanTheorem {
    full_name: String,
}

struct LeanAxiomInput {
    path: PathBuf,
}

impl LeanAxiomInput {
    fn create(lean_root: &Path, theorems: &[LeanTheorem]) -> Result<Self> {
        let mut content = String::from("import Bamti\n");
        for theorem in theorems {
            content.push_str("#print axioms ");
            content.push_str(&theorem.full_name);
            content.push('\n');
        }
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                VerificationError::new(
                    ErrorCode::Io,
                    format!("cannot create Lean axiom audit timestamp: {error}"),
                )
            })?
            .as_nanos();
        for sequence in 0..32u32 {
            let path = env::temp_dir().join(format!(
                "bamts-lean-axioms-{}-{epoch}-{sequence}.lean",
                std::process::id()
            ));
            match fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    if let Err(error) = file.write_all(content.as_bytes()) {
                        let _ = fs::remove_file(&path);
                        return Err(io_error(&path, error));
                    }
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io_error(&path, error)),
            }
        }
        Err(VerificationError::new(
            ErrorCode::Io,
            format!(
                "cannot allocate temporary Lean axiom audit source under {}",
                lean_root.display()
            ),
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LeanAxiomInput {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuintRunsDocument {
    generator_backend: String,
    node_version: String,
    package_lock_sha256: String,
    runs: Vec<QuintRun>,
    schema: String,
    tool: QuintTool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuintTool {
    archive_digest: String,
    archive_digest_algorithm: String,
    name: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuintRun {
    invariants: Vec<String>,
    liveness_witness: Option<LivenessWitness>,
    main_module: String,
    init: String,
    step: String,
    max_samples: u64,
    max_steps: u64,
    model: String,
    seed: String,
    trace: String,
    trace_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LivenessWitness {
    max_samples: u64,
    max_steps: u64,
    name: String,
    seed: String,
    witnessed: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlTool {
    name: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationDocument {
    schema: String,
    tool: ControlTool,
    mutations: Vec<MutationRecord>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MutationRecord {
    model: String,
    invariant: String,
    fault_model: String,
    main_module: String,
    init: String,
    step: String,
    fault_action: String,
    fault_run: String,
    backend: String,
    threads: u64,
    max_steps: u64,
    max_samples: u64,
    seed: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NormalWitnessDocument {
    schema: String,
    tool: ControlTool,
    witnesses: Vec<NormalWitness>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NormalWitness {
    model: String,
    witness_model: String,
    main_module: String,
    init: String,
    step: String,
    invariant: String,
    kind: String,
    antecedent: String,
    backend: String,
    threads: u64,
    max_steps: u64,
    max_samples: u64,
    seed: String,
    run: Option<String>,
    recorded_liveness_witness: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LeanAssumptions {
    schema: String,
    lean_version: String,
    source_sha256: String,
    forbidden_constructs: ForbiddenConstructs,
    observed_kernel_assumptions: Vec<String>,
    user_axioms: Vec<String>,
    public_theorems: Vec<PublicTheorem>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForbiddenConstructs {
    admit: u64,
    axiom: u64,
    implemented_by: u64,
    sorry: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicTheorem {
    file: String,
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let base = env::temp_dir();
            let epoch = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = base.join(format!(
                "bamts-formal-gates-{name}-{}-{epoch}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, content: &str) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn gate_order_accepts_display_order_but_executes_dependencies() {
        assert_eq!(
            ordered_requested_gates(&[Gate::G1, Gate::G2, Gate::G3, Gate::G4, Gate::G5]).unwrap(),
            vec![Gate::G1, Gate::G2, Gate::G5, Gate::G3, Gate::G4]
        );
        assert_eq!(
            ordered_requested_gates(&[Gate::G1, Gate::G2, Gate::G5]).unwrap(),
            vec![Gate::G1, Gate::G2, Gate::G5]
        );
        assert_eq!(
            ordered_requested_gates(&[Gate::G1, Gate::G3])
                .unwrap_err()
                .code(),
            ErrorCode::GateDependency
        );
        assert_eq!(
            ordered_requested_gates(&[Gate::G0]).unwrap_err().code(),
            ErrorCode::Usage
        );
        assert_eq!(
            ordered_requested_gates(&[Gate::G6]).unwrap_err().code(),
            ErrorCode::Usage
        );
    }

    #[test]
    fn duplicate_manifest_keys_are_rejected_before_deserialization() {
        let directory = TestDirectory::new("duplicate-json");
        let path = directory.path().join("runs.json");
        fs::write(&path, "{\"schema\":\"one\",\"schema\":\"two\"}").unwrap();
        assert_eq!(
            parse_strict_json(&path).unwrap_err().code(),
            ErrorCode::Duplicate
        );
    }

    #[test]
    fn quint_witness_parser_requires_the_pinned_output_line() {
        let observed = parse_quint_witness_line(
            "Live was witnessed in 1000 trace(s) out of 1000 explored (100.00%)",
        )
        .unwrap();
        assert_eq!(observed.name, "Live");
        assert_eq!(observed.witnessed, 1000);
        assert_eq!(observed.explored, 1000);
        assert!(
            parse_quint_witness_line("Live was witnessed in 1 trace(s) out of 3 explored (50.00%)")
                .is_err()
        );
        assert!(
            parse_quint_witness_line("Live was witnessed in 1 trace out of 1 explored (100.00%)")
                .is_err()
        );
    }

    #[test]
    fn redex_controls_require_complete_single_authority_test_cases() {
        let directory = TestDirectory::new("redex-controls");
        let first = "control::model.rkt::step::examples";
        let second = "control::model.rkt::step::mutation";
        directory.write(
            "formal/redex/model.rkt",
            &format!(
                "#lang racket\n(test-case\n  \"{first}\"\n  #t)\n(test-case \"{second}\" #t)\n"
            ),
        );
        let catalog = FormalCatalog {
            identifiers: [first.to_owned(), second.to_owned()].into_iter().collect(),
            declared_files: ["model.rkt".to_owned()].into_iter().collect(),
            redex_controls: Some(RedexControls {
                relations: ["model.rkt::step".to_owned()].into_iter().collect(),
                non_vacuity_obligations: ["examples".to_owned(), "mutation".to_owned()]
                    .into_iter()
                    .collect(),
            }),
        };
        assert_eq!(
            validate_redex_catalog_authority(directory.path(), &catalog).unwrap(),
            2
        );

        directory.write(
            "formal/redex/model.rkt",
            &format!("#lang racket\n(test-case \"{first}\" #t)\n"),
        );
        assert_eq!(
            validate_redex_catalog_authority(directory.path(), &catalog)
                .unwrap_err()
                .code(),
            ErrorCode::SetMismatch
        );
    }

    #[test]
    fn missing_mutation_or_normal_witness_controls_are_rejected() {
        let directory = TestDirectory::new("controls");
        directory.write(
            "formal/quint/model.qnt",
            "module model {\n  action init = all {}\n  action step = all {}\n  val Safe = true\n  action faultSafe = all {}\n  run faultSafeRun = init.then(faultSafe).then(checkState(not(Safe)))\n  run normalSafe = init.then(checkState(Safe and antecedent))\n}\n",
        );
        let models = load_quint_models(directory.path()).unwrap();
        let catalog = FormalCatalog {
            identifiers: ["model.qnt::Safe".to_owned()].into_iter().collect(),
            declared_files: ["model.qnt".to_owned()].into_iter().collect(),
            redex_controls: None,
        };
        let mutation = MutationRecord {
            model: "model.qnt".into(),
            invariant: "Safe".into(),
            fault_model: "model.qnt".into(),
            main_module: "model".into(),
            init: "init".into(),
            step: "step".into(),
            fault_action: "faultSafe".into(),
            fault_run: "faultSafeRun".into(),
            backend: "rust".into(),
            threads: 1,
            max_steps: 1,
            max_samples: 1,
            seed: "1".into(),
        };
        let mutation_document = MutationDocument {
            schema: "bamti.quint-mutations/v2".into(),
            tool: ControlTool {
                name: "@informalsystems/quint".into(),
                version: QUINT_VERSION.into(),
            },
            mutations: vec![],
        };
        let witness_document = NormalWitnessDocument {
            schema: "bamti.quint-normal-witnesses/v1".into(),
            tool: ControlTool {
                name: "@informalsystems/quint".into(),
                version: QUINT_VERSION.into(),
            },
            witnesses: vec![],
        };
        assert_eq!(
            validate_control_documents(
                directory.path(),
                &catalog,
                &models,
                &BTreeMap::new(),
                &mutation_document,
                &witness_document
            )
            .unwrap_err()
            .code(),
            ErrorCode::SetMismatch
        );

        let mutation_document = MutationDocument {
            mutations: vec![mutation],
            ..mutation_document
        };
        assert_eq!(
            validate_control_documents(
                directory.path(),
                &catalog,
                &models,
                &BTreeMap::new(),
                &mutation_document,
                &witness_document
            )
            .unwrap_err()
            .code(),
            ErrorCode::SetMismatch
        );
    }

    #[cfg(unix)]
    #[test]
    fn tool_failure_propagates_bounded_child_output() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("tool-failure");
        let tool = directory.path().join("failing-tool");
        fs::write(&tool, "#!/bin/sh\nprintf 'fixture failure' >&2\nexit 17\n").unwrap();
        let mut permissions = fs::metadata(&tool).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&tool, permissions).unwrap();

        let error = run_tool(directory.path(), &tool, &[], "fixture tool").unwrap_err();
        assert_eq!(error.code(), ErrorCode::ToolFailed);
        assert!(error.to_string().contains("fixture failure"));
    }

    #[test]
    fn racket_source_code_strips_line_comments_before_counters() {
        // A line-commented test-case must not be counted as an authority control.
        let source = "#lang racket\n; (test-case \"control::model.rkt::step::examples\" #t)\n(test-case \"control::model.rkt::step::examples\" #t)\n";
        let stripped = racket_source_code(source);
        assert_eq!(
            racket_test_case_count(&stripped, "control::model.rkt::step::examples"),
            1
        );
    }

    #[test]
    fn racket_source_code_strips_line_commented_rule_labels() {
        // A line-commented rule label must not be counted as authority.
        let source = "#lang racket\n(define-judgment-form model\n  #:mode (step I I O)\n  #:contract (step expr env val))\n; (redex-match model \"rule::step::beta\" (term e))\n(redex-match model \"rule::step::beta\" (term e))\n(module+ test\n  (test-case \"control::model.rkt::step::examples\" #t))\n";
        let stripped = racket_source_code(source);
        let authority = stripped
            .split_once("(module+ test")
            .map(|(authority, _)| authority)
            .unwrap_or(&stripped);
        assert_eq!(quoted_label_count(authority, "rule::step::beta"), 1);
    }

    #[test]
    fn racket_source_code_preserves_semicolons_inside_quoted_labels() {
        // A semicolon inside a quoted label/string is preserved so the label still counts.
        let source = "#lang racket\n(redex-match model \"rule::step;semi\" (term e))\n(module+ test\n  (test-case \"control::model.rkt::step;semi::examples\" #t))\n";
        let stripped = racket_source_code(source);
        let authority = stripped
            .split_once("(module+ test")
            .map(|(authority, _)| authority)
            .unwrap_or(&stripped);
        assert_eq!(quoted_label_count(authority, "rule::step;semi"), 1);
        assert_eq!(
            racket_test_case_count(&stripped, "control::model.rkt::step;semi::examples"),
            1
        );
    }

    #[test]
    fn racket_source_code_keeps_text_past_escaped_quote_semicolon() {
        // An escaped quote before a semicolon must not end the string, so a later
        // real declaration on the same logical line is not swallowed by the comment.
        let source =
            "#lang racket\n(define (helper) (displayln \"a\\\";b\"))\n(define (target) #t)\n";
        let stripped = racket_source_code(source);
        assert_eq!(racket_declaration_count(&stripped, "target"), 1);
        assert_eq!(racket_declaration_count(&stripped, "helper"), 1);
    }

    #[test]
    fn racket_test_case_count_counts_multiline_test_case_labels_once() {
        // A test-case whose opener and quoted label span separate physical lines
        // still counts exactly once.
        let source = "#lang racket\n(test-case\n  \"control::model.rkt::step::examples\"\n  #t)\n";
        let stripped = racket_source_code(source);
        assert_eq!(
            racket_test_case_count(&stripped, "control::model.rkt::step::examples"),
            1
        );

        // A second, distinct control on its own line is counted independently.
        let source = "#lang racket\n(test-case\n  \"control::model.rkt::step::examples\"\n  #t)\n(test-case \"control::model.rkt::step::mutation\" #t)\n";
        let stripped = racket_source_code(source);
        assert_eq!(
            racket_test_case_count(&stripped, "control::model.rkt::step::examples"),
            1
        );
        assert_eq!(
            racket_test_case_count(&stripped, "control::model.rkt::step::mutation"),
            1
        );
    }
}
