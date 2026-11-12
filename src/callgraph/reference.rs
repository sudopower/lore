use super::*;

#[sqlx::test(migrations = "./migrations")]
async fn sql_name_fast_path_matches_rust_rightmost_delimiters(pool: PgPool) {
    let entries = strings(&[
        "",
        ":",
        "::",
        ":::",
        "::::",
        "plain",
        "::foo",
        "Plan::foo",
        "a:b",
        "a::",
        "a:::",
        "a::::",
        "a:::b",
        "a::::b",
        ":::foo",
        "A::B::foo",
        "a::b::",
        "測試",
        "測試:::é",
        "é::測試",
        "::🦀",
    ]);
    let expected: Vec<String> = entries.iter().map(|s| called_name(s).to_string()).collect();
    let actual: Vec<String> = sqlx::query_scalar(
        "SELECT CASE WHEN strpos(entry,'::') = 0 THEN entry
                     ELSE reverse(split_part(reverse(entry),'::',1)) END
         FROM unnest($1::text[]) WITH ORDINALITY AS entries(entry,ord) ORDER BY ord",
    )
    .bind(&entries)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(actual, expected);
}

// Original related-ID algorithm, retained as the differential oracle for slim projections.
async fn related_reference(
    connection: &mut PgConnection,
    scope: &Scope<'_>,
    query: &str,
) -> Result<Vec<i64>, AppError> {
    let symbols = query_symbols(query);
    if symbols.is_empty() {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = symbols.iter().map(|s| s.name.clone()).collect();
    names.dedup();
    let defs = sites_with(connection, scope, "defines", &names, RELATED_BRANCH).await?;
    if defs.len() == RELATED_BRANCH as usize {
        return Ok(Vec::new());
    }

    let mut out: Vec<i64> = Vec::new();
    // (name, the indexes in `defs` of all its definitions, the ids of those the query means)
    let mut matched: Vec<(&str, Vec<usize>, HashSet<i64>)> = Vec::new();
    for symbol in &symbols {
        let name = symbol.name.as_str();
        let all: Vec<usize> = (0..defs.len())
            .filter(|&i| defs[i].defines.iter().any(|d| d == name))
            .collect();
        let meant: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&i| match &symbol.qualifier {
                Some(q) => qualifier_matches(q, name, &defs[i]),
                None => true,
            })
            .collect();
        if meant.is_empty() || meant.len() > RELATED_DEFINITIONS_PER_NAME {
            continue;
        }
        out.extend(meant.iter().map(|&i| defs[i].chunk_id));
        matched.push((name, all, meant.iter().map(|&i| defs[i].chunk_id).collect()));
    }
    if matched.is_empty() {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::new();
    out.retain(|id| seen.insert(*id));
    if out.len() >= RELATED_LIMIT {
        out.truncate(RELATED_LIMIT);
        return Ok(out);
    }

    let caller_keys: Vec<String> = matched
        .iter()
        .flat_map(|(name, _, _)| call_keys(name))
        .collect();
    let matched_names: HashSet<&str> = matched.iter().map(|(n, _, _)| *n).collect();
    let mut callee_names: Vec<String> = Vec::new();
    let mut callee_seen = HashSet::new();
    'names: for (_, _, meant) in &matched {
        for def in defs.iter().filter(|d| meant.contains(&d.chunk_id)) {
            for entry in &def.calls {
                if callee_names.len() == RELATED_CALLEE_NAMES {
                    break 'names;
                }
                let name = called_name(entry);
                if !matched_names.contains(name) && callee_seen.insert(name) {
                    callee_names.push(name.to_string());
                }
            }
        }
    }
    // Reuse a pooled connection rather than opening another one on the first search.
    let callers = sites_with(connection, scope, "calls", &caller_keys, RELATED_BRANCH).await?;

    for caller in &callers {
        for (name, all, meant) in &matched {
            let candidates: Vec<&Site> = all.iter().map(|&i| &defs[i]).collect();
            if let Some((picks, resolution)) = resolve(name, caller, &candidates)
                && resolution != Resolution::Ambiguous
                && picks
                    .iter()
                    .any(|&i| meant.contains(&candidates[i].chunk_id))
                && !picks
                    .iter()
                    .all(|&i| candidates[i].chunk_id == caller.chunk_id)
            {
                out.push(caller.chunk_id);
            }
        }
    }
    seen.clear();
    out.retain(|id| seen.insert(*id));
    if out.len() >= RELATED_LIMIT {
        out.truncate(RELATED_LIMIT);
        return Ok(out);
    }
    let callees = sites_with(connection, scope, "defines", &callee_names, RELATED_BRANCH).await?;
    if !callees.is_empty() && callees.len() < RELATED_BRANCH as usize {
        for (_, _, meant) in &matched {
            for def in defs.iter().filter(|d| meant.contains(&d.chunk_id)) {
                for name in def.calls.iter().map(|e| called_name(e)) {
                    let candidates: Vec<&Site> = callees
                        .iter()
                        .filter(|c| c.defines.iter().any(|d| d == name))
                        .collect();
                    if let Some((picks, resolution)) = resolve(name, def, &candidates)
                        && resolution != Resolution::Ambiguous
                    {
                        out.extend(picks.iter().map(|&i| candidates[i].chunk_id));
                    }
                }
            }
        }
    }

    seen.clear();
    out.retain(|id| seen.insert(*id));
    out.truncate(RELATED_LIMIT);
    Ok(out)
}

async fn setup(pool: &PgPool, name: &str) -> (Uuid, Uuid) {
    let tenant = Uuid::new_v4();
    let source = Uuid::new_v4();
    sqlx::query("INSERT INTO tenants(id,name) VALUES($1,$2)")
        .bind(tenant)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sources(id,tenant_id,kind,name) VALUES($1,$2,'repo','app')")
        .bind(source)
        .bind(tenant)
        .execute(pool)
        .await
        .unwrap();
    (tenant, source)
}

async fn add(
    pool: &PgPool,
    tenant: Uuid,
    source: Uuid,
    path: &str,
    defines: &[String],
    calls: &[String],
) -> i64 {
    let document = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO documents(id,tenant_id,source_id,external_id,kind,title,metadata,content_hash)
         VALUES($1,$2,$3,$4,'code',$4,jsonb_build_object('path',$4::text),'reference-fixture')",
    )
    .bind(document)
    .bind(tenant)
    .bind(source)
    .bind(path)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO chunks(tenant_id,document_id,ord,text,tsv,embedding,defines,calls)
         VALUES($1,$2,0,'fixture',to_tsvector('simple','fixture'),
                array_fill(0::real,ARRAY[384])::vector,$3,$4) RETURNING id",
    )
    .bind(tenant)
    .bind(document)
    .bind(defines)
    .bind(calls)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

async fn compare(pool: &PgPool, scope: Scope<'_>, query: &str) -> Vec<i64> {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .unwrap();
    let original = related_reference(&mut tx, &scope, query).await.unwrap();
    let slim = related_on(&mut tx, &scope, query).await.unwrap();
    assert_eq!(slim, original, "{query}, {scope:?}");
    tx.rollback().await.unwrap();
    slim
}

#[sqlx::test(migrations = "./migrations")]
async fn projections_preserve_qualified_callers_ambiguity_and_scope(pool: PgPool) {
    let (tenant, source) = setup(&pool, "projection-scopes").await;
    let typed = add(
        &pool,
        tenant,
        source,
        "a/type.rs",
        &strings(&["target", "Type::target"]),
        &strings(&["Other::tail", "::tail"]),
    )
    .await;
    add(
        &pool,
        tenant,
        source,
        "b/other.rs",
        &strings(&["target", "Other::target"]),
        &[],
    )
    .await;
    let mut calls = vec!["irrelevant".to_string(); 254];
    calls.extend(strings(&["Type::target", "::target"]));
    let caller = add(&pool, tenant, source, "c/caller.rs", &[], &calls).await;
    add(
        &pool,
        tenant,
        source,
        "d/ambiguous.rs",
        &[],
        &strings(&["target"]),
    )
    .await;
    add(
        &pool,
        tenant,
        source,
        "e/wrong.rs",
        &[],
        &strings(&["Other::target", "::target"]),
    )
    .await;
    let tail = add(
        &pool,
        tenant,
        source,
        "f/tail.rs",
        &strings(&["tail", "Other::tail"]),
        &[],
    )
    .await;
    let (private, private_source) = setup(&pool, "projection-private").await;
    add(
        &pool,
        private,
        private_source,
        "private.rs",
        &strings(&["target"]),
        &strings(&["target"]),
    )
    .await;
    let other_source = Uuid::new_v4();
    sqlx::query("INSERT INTO sources(id,tenant_id,kind,name) VALUES($1,$2,'repo','other')")
        .bind(other_source)
        .bind(tenant)
        .execute(&pool)
        .await
        .unwrap();
    add(
        &pool,
        tenant,
        other_source,
        "foreign.rs",
        &[],
        &strings(&["Type::target", "::target"]),
    )
    .await;

    let scope = Scope {
        tenant_id: tenant,
        kinds: None,
        source: Some("app"),
    };
    assert_eq!(
        compare(&pool, scope, "Type::target").await,
        vec![typed, caller, tail]
    );
    for query in ["target", "Other::target", "tail", "not_indexed"] {
        compare(&pool, scope, query).await;
        compare(
            &pool,
            Scope {
                source: None,
                ..scope
            },
            query,
        )
        .await;
    }
    let kinds = strings(&["doc"]);
    assert!(
        compare(
            &pool,
            Scope {
                kinds: Some(&kinds),
                ..scope
            },
            "Type::target"
        )
        .await
        .is_empty()
    );
    assert!(
        compare(
            &pool,
            Scope {
                source: Some("absent"),
                ..scope
            },
            "Type::target"
        )
        .await
        .is_empty()
    );
    assert!(
        compare(
            &pool,
            Scope {
                tenant_id: Uuid::new_v4(),
                ..scope
            },
            "Type::target"
        )
        .await
        .is_empty()
    );
    compare(
        &pool,
        Scope {
            tenant_id: private,
            ..scope
        },
        "target",
    )
    .await;
}

#[sqlx::test(migrations = "./migrations")]
async fn global_callee_budget_preserves_duplicates_and_co_defined_names(pool: PgPool) {
    let (tenant, source) = setup(&pool, "projection-global-budget").await;
    let mut defines = vec!["unrelated_definition".to_string(); 254];
    defines.extend(strings(&["root_fn", "Scope::root_fn"]));
    let mut calls = vec!["root_fn".to_string(); 8];
    for _ in 0..32 {
        calls.extend(strings(&["Wrong::picked", "::picked"]));
    }
    calls.extend((0..63).map(|n| format!("filler_{n}")));
    calls.extend(strings(&["co_defined", "beyond_64"]));
    let root = add(&pool, tenant, source, "a/root.rs", &defines, &calls).await;
    let picked = add(
        &pool,
        tenant,
        source,
        "b/right.rs",
        &strings(&["picked", "Right::picked", "co_defined"]),
        &[],
    )
    .await;
    let boundary = add(
        &pool,
        tenant,
        source,
        "c/boundary.rs",
        &strings(&["filler_62"]),
        &[],
    )
    .await;
    let beyond = add(
        &pool,
        tenant,
        source,
        "d/beyond.rs",
        &strings(&["beyond_64"]),
        &[],
    )
    .await;
    let scope = Scope {
        tenant_id: tenant,
        kinds: None,
        source: Some("app"),
    };
    // The first selected name cannot resolve Wrong::picked. A later, unselected co-defined
    // name can still reach that same returned chunk. D must retain that source call.
    let ids = compare(&pool, scope, "root_fn").await;
    assert_eq!(ids, vec![root, boundary, picked]);
    assert!(!ids.contains(&beyond));
    // Revisiting one definition through a second matched name cannot move the global budget.
    compare(&pool, scope, "root_fn Scope::root_fn").await;
}

#[sqlx::test(migrations = "./migrations")]
async fn projections_preserve_initial_caller_and_callee_row_caps(pool: PgPool) {
    let (tenant, source) = setup(&pool, "projection-row-caps").await;
    let root = add(
        &pool,
        tenant,
        source,
        "a/root.rs",
        &strings(&["root_fn"]),
        &strings(&["chosen::wide", "::wide"]),
    )
    .await;
    let chosen = add(
        &pool,
        tenant,
        source,
        "b/chosen.rs",
        &strings(&["wide"]),
        &[],
    )
    .await;
    let mut last = 0;
    for n in 0..49 {
        last = add(
            &pool,
            tenant,
            source,
            &format!("c/other{n:02}.rs"),
            &strings(&["wide"]),
            &[],
        )
        .await;
    }
    let scope = Scope {
        tenant_id: tenant,
        kinds: None,
        source: Some("app"),
    };
    assert_eq!(
        compare(&pool, scope, "root_fn").await,
        vec![root],
        "50 callee candidates are discarded"
    );
    sqlx::query("DELETE FROM chunks WHERE id=$1")
        .bind(last)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        compare(&pool, scope, "root_fn").await,
        vec![root, chosen],
        "49 candidates retain qualified resolution"
    );

    let target = add(
        &pool,
        tenant,
        source,
        "d/target.rs",
        &strings(&["caller_target"]),
        &[],
    )
    .await;
    for n in 0..50 {
        add(
            &pool,
            tenant,
            source,
            &format!("e/wrong{n:02}.rs"),
            &[],
            &strings(&["Wrong::caller_target", "::caller_target"]),
        )
        .await;
    }
    let late = add(
        &pool,
        tenant,
        source,
        "z/late.rs",
        &[],
        &strings(&["caller_target"]),
    )
    .await;
    let ids = compare(&pool, scope, "caller_target").await;
    assert_eq!(
        ids,
        vec![target],
        "caller cap applies before resolving/omitting wrong qualifiers"
    );
    assert!(!ids.contains(&late));

    for n in 0..50 {
        add(
            &pool,
            tenant,
            source,
            &format!("saturated/{n:02}.rs"),
            &strings(&[
                "saturated",
                if n == 0 {
                    "Chosen::saturated"
                } else {
                    "Other::saturated"
                },
            ]),
            &[],
        )
        .await;
    }
    assert!(
        compare(&pool, scope, "Chosen::saturated").await.is_empty(),
        "initial 50-row cap applies before narrowing a qualified subset"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn definition_budget_avoids_loading_large_unused_calls(pool: PgPool) {
    let (tenant, source) = setup(&pool, "projection-output-budget").await;
    for n in 0..40 {
        let mut defines = vec!["irrelevant".to_string(); 255];
        defines.push(format!("heavy_{}", n % 8));
        let calls = vec!["unrelated".repeat(14); 256];
        add(
            &pool,
            tenant,
            source,
            &format!("heavy/{n:02}.rs"),
            &defines,
            &calls,
        )
        .await;
    }
    let scope = Scope {
        tenant_id: tenant,
        kinds: None,
        source: Some("app"),
    };
    let query = "heavy_0 heavy_1 heavy_2 heavy_3 heavy_4 heavy_5 heavy_6 heavy_7";
    let ids = compare(&pool, scope, query).await;
    assert_eq!(ids.len(), RELATED_LIMIT);
    let names: Vec<String> = query_symbols(query)
        .iter()
        .map(|s| s.name.clone())
        .collect();
    let mut connection = pool.acquire().await.unwrap();
    let projected = related_sites(
        &mut connection,
        &scope,
        &names,
        RelatedProjection::Definitions(&names),
    )
    .await
    .unwrap();
    assert_eq!(projected.len(), 40);
    assert!(
        projected
            .iter()
            .all(|s| s.calls.is_empty() && s.defines.len() == 1)
    );
}
