use crate::app::AppState;
use anyhow::Result;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};

pub async fn run_fixtures(state: AppState) -> Result<()> {
    println!("Initializing fixtures...");
    let db = state.db();

    // 1. Initialize Extensions and Routes
    init_core_fixtures(db).await?;

    // 2. Initialize Demo User for demo mode
    if state.config().demo_mode {
        crate::models::user::Model::upsert_super_user(
            db,
            "demo@miuda.ai",
            "demo@miuda.ai",
            "hello@miuda.ai",
        )
        .await?;
        println!("Created demo superuser: demo@miuda.ai");
    }

    // 3. Initialize Addon Fixtures (calling each addon's seed_fixtures)
    state
        .addon_registry
        .seed_all_fixtures(state.clone())
        .await?;

    Ok(())
}

async fn init_core_fixtures(db: &sea_orm::DatabaseConnection) -> Result<()> {
    use crate::models::extension;
    use crate::models::routing;

    // Extensions
    for ext_num in ["1001", "1002"] {
        if extension::Entity::find()
            .filter(extension::Column::Extension.eq(ext_num))
            .one(db)
            .await?
            .is_none()
        {
            let ext = extension::ActiveModel {
                extension: Set(ext_num.to_string()),
                display_name: Set(Some(format!("Extension {}", ext_num))),
                sip_password: Set(Some("1234".to_string())),
                ..Default::default()
            };
            ext.insert(db).await?;
            println!("Created extension {}", ext_num);
        }
    }

    // Routing
    if routing::Entity::find()
        .filter(routing::Column::Name.eq("Default Outbound"))
        .one(db)
        .await?
        .is_none()
    {
        let route = routing::ActiveModel {
            name: Set("Default Outbound".to_string()),
            direction: Set(routing::RoutingDirection::Outbound),
            destination_pattern: Set(Some("^\\d+$".to_string())),
            ..Default::default()
        };
        route.insert(db).await?;
        println!("Created default outbound route");
    }

    Ok(())
}
