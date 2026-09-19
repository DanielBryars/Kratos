use sqlx::PgPool;

/// Grants the application identity data access to all current and future public-schema objects.
///
/// The caller must be the migration owner. Identifier quoting is delegated to `PostgreSQL` so an IAM
/// database username containing punctuation cannot change the statements' structure.
///
/// # Errors
///
/// Returns the underlying `PostgreSQL` error when quoting, a grant, or the transaction fails.
pub async fn grant_application_privileges(
    pool: &PgPool,
    application_user: &str,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    let quoted_user: String = sqlx::query_scalar("SELECT quote_ident($1)")
        .bind(application_user)
        .fetch_one(&mut *transaction)
        .await?;
    let quoted_database: String = sqlx::query_scalar("SELECT quote_ident(current_database())")
        .fetch_one(&mut *transaction)
        .await?;
    let statements = [
        format!("GRANT CONNECT ON DATABASE {quoted_database} TO {quoted_user}"),
        format!("GRANT USAGE ON SCHEMA public TO {quoted_user}"),
        format!(
            "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO {quoted_user}"
        ),
        format!("GRANT USAGE, SELECT, UPDATE ON ALL SEQUENCES IN SCHEMA public TO {quoted_user}"),
        format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {quoted_user}"
        ),
        format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT USAGE, SELECT, UPDATE ON SEQUENCES TO {quoted_user}"
        ),
    ];
    for statement in statements {
        sqlx::query(&statement).execute(&mut *transaction).await?;
    }
    transaction.commit().await
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::grant_application_privileges;

    #[sqlx::test(migrations = "./migrations")]
    async fn application_role_can_modify_current_and_future_tables(pool: PgPool) {
        let role = format!("runtime.{}@example", Uuid::new_v4().simple());
        let quoted_role: String = sqlx::query_scalar("SELECT quote_ident($1)")
            .bind(&role)
            .fetch_one(&pool)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE ROLE {quoted_role}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE grant_test_existing (id integer PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();

        grant_application_privileges(&pool, &role).await.unwrap();
        sqlx::query("CREATE TABLE grant_test_future (id integer PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();

        let mut connection = pool.acquire().await.unwrap();
        sqlx::query(&format!("SET ROLE {quoted_role}"))
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO grant_test_existing VALUES (1)")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("UPDATE grant_test_existing SET id = 2 WHERE id = 1")
            .execute(&mut *connection)
            .await
            .unwrap();
        let value: i32 = sqlx::query_scalar("SELECT id FROM grant_test_existing")
            .fetch_one(&mut *connection)
            .await
            .unwrap();
        assert_eq!(value, 2);
        sqlx::query("DELETE FROM grant_test_existing WHERE id = 2")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query("INSERT INTO grant_test_future VALUES (1)")
            .execute(&mut *connection)
            .await
            .unwrap();
    }
}
