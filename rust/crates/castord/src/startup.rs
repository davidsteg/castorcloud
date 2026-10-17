//! What castord sets up once when it starts: the
//! self-registration flow in Authentik and the local node.

use sqlx::PgPool;

use crate::Control;
use crate::node::{Node, NodeRow};

/// Sign-up with a verified e-mail when Authentik can send mail
/// (AUTHENTIK_EMAIL__HOST), else the two-stage flow; then our blueprint
/// that points the brand at it.
pub async fn enrollment(c: &Control, with_email: bool) -> Result<(), String> {
    let path = if with_email {
        "example/flows-enrollment-email-verification.yaml"
    } else {
        "example/flows-enrollment-2-stage.yaml"
    };
    c.authentik
        .ensure_blueprint_instance(
            "personal-cloud-enrollment",
            path,
            "default-enrollment-flow",
            Some("personal-cloud-signup"),
        )
        .await
}

/// The Docker host castord runs on, as the node "local". It takes tenants
/// only when `enabled` (LOCAL_NODE isn't "off": development); production
/// places them on registered nodes. An enabled local node is prepared now.
pub async fn local_node(c: &Control, db: &PgPool, enabled: bool) -> Result<(), String> {
    let id: String = sqlx::query_scalar(
        r#"INSERT INTO "Node" (id, name, kind, "sshHost", capacity, enabled)
           VALUES ($1, 'local', 'LOCAL', 'local', 50, $2)
           ON CONFLICT (name) DO UPDATE SET enabled = EXCLUDED.enabled
           RETURNING id"#,
    )
    .bind(crate::db::new_id()?)
    .bind(enabled)
    .fetch_one(db)
    .await
    .map_err(|e| format!("local node: {e}"))?;
    if enabled {
        eprintln!("castord: preparing the local node");
        let node = Node::new(&NodeRow::load(db, &id).await?, &c.root, c.exec.clone())?;
        crate::apply::prepare_once(c, &node).await?;
    }
    Ok(())
}
