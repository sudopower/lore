use super::*;

// Independent scalar cosine reference, not the SQL operator or the ANN candidate set.
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(&a, &b)| f64::from(a) * f64::from(b))
        .sum();
    let norm = |v: &[f32]| v.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>().sqrt();
    1.0 - dot / (norm(a) * norm(b))
}

#[sqlx::test(migrations = "./migrations")]
async fn source_scoped_vectors_match_scalar_cosine_with_filters_and_ties(pool: PgPool) {
    let tenant = Uuid::new_v4();
    let other = Uuid::new_v4();
    for id in [tenant, other] {
        sqlx::query("INSERT INTO tenants(id,name) VALUES($1,$2)")
            .bind(id)
            .bind(id.to_string())
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut rows = Vec::new();
    for (owner, source, kind, count, axis) in [
        (other, "wanted", "code", 1, 0),
        (tenant, "excluded", "code", 1, 0),
        (tenant, "wanted", "code", 65, 0),
        (tenant, "wanted", "doc", 2, 1),
    ] {
        let source_id = Uuid::new_v4();
        let document_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO sources(id,tenant_id,kind,name) VALUES($1,$2,'repo',$3)
                    ON CONFLICT(tenant_id,kind,name) DO UPDATE SET name=EXCLUDED.name
                    RETURNING id",
        )
        .bind(source_id)
        .bind(owner)
        .bind(source)
        .fetch_one(&pool)
        .await
        .unwrap();
        let source_id: Uuid =
            sqlx::query_scalar("SELECT id FROM sources WHERE tenant_id=$1 AND name=$2")
                .bind(owner)
                .bind(source)
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO documents(id,tenant_id,source_id,external_id,kind,title,content_hash)
                    VALUES($1,$2,$3,$4,$5,'fixture','fixture')",
        )
        .bind(document_id)
        .bind(owner)
        .bind(source_id)
        .bind(document_id.to_string())
        .bind(kind)
        .execute(&pool)
        .await
        .unwrap();
        for ord in 0..count {
            let mut embedding = vec![0.0_f32; 384];
            embedding[axis] = 1.0;
            let enumerates = ord % 7 == 0;
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO chunks(tenant_id,document_id,ord,text,tsv,embedding,enumerates)
                 VALUES($1,$2,$3,'fixture',to_tsvector('simple','fixture'),$4,$5) RETURNING id",
            )
            .bind(owner)
            .bind(document_id)
            .bind(ord)
            .bind(Vector::from(embedding.clone()))
            .bind(enumerates)
            .fetch_one(&pool)
            .await
            .unwrap();
            rows.push((id, owner, source, kind, embedding, enumerates));
        }
    }
    let doc = ["doc".to_string()];
    let code = ["code".to_string()];
    let absent_kind = ["note".to_string()];
    let mut tx = pool.begin().await.unwrap();
    for mode in ["force_custom_plan", "force_generic_plan"] {
        sqlx::query(&format!("SET LOCAL plan_cache_mode={mode}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        for axis in 0..2 {
            let mut query = vec![0.0_f32; 384];
            query[axis] = 1.0;
            for (owner, source, kinds) in [
                (tenant, "wanted", None),
                (tenant, "wanted", Some(doc.as_slice())),
                (tenant, "wanted", Some(code.as_slice())),
                (tenant, "wanted", Some(absent_kind.as_slice())),
                (tenant, "absent", None),
                (other, "wanted", None),
                (Uuid::nil(), "wanted", None),
            ] {
                let mut expected: Vec<_> = rows
                    .iter()
                    .filter(|r| {
                        r.1 == owner
                            && r.2 == source
                            && kinds.is_none_or(|k| k.iter().any(|s| s == r.3))
                    })
                    .map(|r| (cosine(&query, &r.4), r.0, r.5))
                    .collect();
                expected.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                let expected: Vec<_> = expected
                    .into_iter()
                    .take(BRANCH_LIMIT as usize)
                    .map(|(_, id, enumerates)| (id, enumerates))
                    .collect();
                let actual = vector_branch(
                    &mut tx,
                    query.clone(),
                    &Filter {
                        tenant_id: owner,
                        source: Some(source),
                        kinds,
                    },
                )
                .await
                .unwrap();
                assert_eq!(
                    actual, expected,
                    "{mode} axis={axis} source={source} kinds={kinds:?}"
                );
            }
        }
    }
}
