use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{big_integer, integer, string, timestamp};
use sea_orm_migration::sea_query::{ColumnDef, ForeignKeyAction};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "wholesale_sales_agents")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    pub user_id: i64,
    pub name: String,
    pub max_discount_bps: i32,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "tenant_discount::Entity")]
    TenantDiscounts,
}

impl Related<tenant_discount::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::TenantDiscounts.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

pub mod tenant_discount {
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "wholesale_agent_tenant_discounts")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i64,
        pub agent_id: i64,
        pub tenant_id: i64,
        pub discount_bps: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::Entity",
            from = "Column::AgentId",
            to = "super::Column::Id"
        )]
        Agent,
    }

    impl Related<super::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Agent.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Entity)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Column::Id)
                            .big_integer()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(big_integer(Column::UserId))
                    .col(string(Column::Name).char_len(200))
                    .col(integer(Column::MaxDiscountBps).default(0))
                    .col(timestamp(Column::CreatedAt).default(Expr::current_timestamp()))
                    .col(timestamp(Column::UpdatedAt).default(Expr::current_timestamp()))
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(tenant_discount::Entity)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(tenant_discount::Column::Id)
                            .big_integer()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(big_integer(tenant_discount::Column::AgentId))
                    .col(big_integer(tenant_discount::Column::TenantId))
                    .col(integer(tenant_discount::Column::DiscountBps).default(0))
                    .foreign_key(
                        ForeignKey::create()
                            .from(tenant_discount::Entity, tenant_discount::Column::AgentId)
                            .to(Entity, Column::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(tenant_discount::Entity).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Entity).to_owned())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::migration::Migrator;
    use sea_orm::{ActiveValue::Set, Database, EntityTrait};
    use sea_orm_migration::MigratorTrait;

    async fn setup_db() -> sea_orm::DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect sqlite memory");
        Migrator::up(&db, None).await.expect("run migrations");
        db
    }

    #[tokio::test]
    async fn agent_table_created() {
        let db = setup_db().await;
        let agents = Entity::find().all(&db).await.expect("query agents");
        assert_eq!(agents.len(), 0);
    }

    #[tokio::test]
    async fn tenant_discount_table_created() {
        let db = setup_db().await;
        let discounts = tenant_discount::Entity::find()
            .all(&db)
            .await
            .expect("query discounts");
        assert_eq!(discounts.len(), 0);
    }

    #[tokio::test]
    async fn insert_and_query_agent() {
        let db = setup_db().await;
        let now = chrono::Utc::now();
        let agent = ActiveModel {
            user_id: Set(10),
            name: Set("Agent One".to_string()),
            max_discount_bps: Set(2000),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        let inserted = Entity::insert(agent)
            .exec_with_returning(&db)
            .await
            .expect("insert agent");
        assert_eq!(inserted.user_id, 10);
        assert_eq!(inserted.name, "Agent One");
        assert_eq!(inserted.max_discount_bps, 2000);
    }

    #[tokio::test]
    async fn insert_tenant_discount_with_cascade_delete() {
        let db = setup_db().await;
        let now = chrono::Utc::now();
        let agent = ActiveModel {
            user_id: Set(20),
            name: Set("Agent Two".to_string()),
            max_discount_bps: Set(1500),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        let inserted_agent = Entity::insert(agent)
            .exec_with_returning(&db)
            .await
            .expect("insert agent");

        let discount = tenant_discount::ActiveModel {
            agent_id: Set(inserted_agent.id),
            tenant_id: Set(100),
            discount_bps: Set(1000),
            ..Default::default()
        };
        tenant_discount::Entity::insert(discount)
            .exec(&db)
            .await
            .expect("insert discount");

        let discounts = tenant_discount::Entity::find()
            .all(&db)
            .await
            .expect("query discounts");
        assert_eq!(discounts.len(), 1);
        assert_eq!(discounts[0].agent_id, inserted_agent.id);
        assert_eq!(discounts[0].tenant_id, 100);
        assert_eq!(discounts[0].discount_bps, 1000);

        Entity::delete_by_id(inserted_agent.id)
            .exec(&db)
            .await
            .expect("delete agent");
        let remaining = tenant_discount::Entity::find()
            .all(&db)
            .await
            .expect("query remaining discounts");
        assert_eq!(remaining.len(), 0);
    }
}
