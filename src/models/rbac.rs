use sea_orm::Set;
use sea_orm::entity::prelude::*;
use sea_orm_migration::prelude::*;
use sea_orm_migration::schema::{big_integer, boolean, string, timestamp};
use sea_orm_migration::sea_query::{ColumnDef, ForeignKeyAction};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "rustpbx_roles")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = true)]
    pub id: i64,
    #[sea_orm(unique)]
    pub name: String,
    pub description: String,
    pub is_system: bool,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "role_permission::Entity")]
    RolePermissions,
    #[sea_orm(has_many = "user_role::Entity")]
    UserRoles,
}

impl Related<role_permission::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::RolePermissions.def()
    }
}

impl Related<user_role::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::UserRoles.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}

pub mod role_permission {
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "rustpbx_role_permissions")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i64,
        pub role_id: i64,
        pub resource: String,
        pub action: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::Entity",
            from = "Column::RoleId",
            to = "super::Column::Id"
        )]
        Role,
    }

    impl Related<super::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Role.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod user_role {
    use chrono::Utc;
    use sea_orm::ActiveValue::Set;
    use sea_orm::entity::prelude::*;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
    #[sea_orm(table_name = "rustpbx_user_roles")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = true)]
        pub id: i64,
        pub user_id: i64,
        pub role_id: i64,
        pub created_at: DateTimeUtc,
    }

    impl Model {
        pub fn new(user_id: i64, role_id: i64) -> ActiveModel {
            ActiveModel {
                user_id: Set(user_id),
                role_id: Set(role_id),
                created_at: Set(Utc::now()),
                ..Default::default()
            }
        }
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {
        #[sea_orm(
            belongs_to = "super::Entity",
            from = "Column::RoleId",
            to = "super::Column::Id"
        )]
        Role,
    }

    impl Related<super::Entity> for Entity {
        fn to() -> RelationDef {
            Relation::Role.def()
        }
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub const SYSTEM_ROLES: &[(&str, &str)] = &[
    ("superadmin", "Super administrator with all permissions"),
    ("admin", "System administrator for day-to-day management"),
    ("supervisor", "Call center supervisor monitoring agents"),
    ("operator", "Operator with view and call control access"),
    ("viewer", "Read-only auditor for compliance"),
    ("ivr_editor", "IVR flow editor"),
    ("wholesale_admin", "Wholesale tenant and billing manager"),
];

const ROLE_PERMISSIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "superadmin",
        &[
            ("system", "read"),
            ("system", "write"),
            ("users", "manage"),
            ("departments", "write"),
            ("extensions", "read"),
            ("extensions", "write"),
            ("extensions", "delete"),
            ("trunks", "read"),
            ("trunks", "write"),
            ("routes", "read"),
            ("routes", "write"),
            ("queues", "read"),
            ("queues", "write"),
            ("queues", "realtime"),
            ("ivr", "read"),
            ("ivr", "write"),
            ("ivr", "publish"),
            ("cdr", "read"),
            ("cdr", "read:recording"),
            ("cdr", "delete"),
            ("cdr", "export"),
            ("calls", "read"),
            ("calls", "control"),
            ("ami", "access"),
            ("voicemail", "read"),
            ("voicemail", "write"),
            ("voicemail", "settings"),
            ("endpoints", "read"),
            ("endpoints", "write"),
            ("endpoints", "reboot"),
            ("wholesale", "read"),
            ("wholesale", "write"),
            ("wholesale", "billing"),
            ("wholesale", "settings"),
            ("metrics", "read"),
            ("diagnostics", "read"),
        ],
    ),
    (
        "admin",
        &[
            ("system", "read"),
            ("users", "manage"),
            ("departments", "write"),
            ("extensions", "read"),
            ("extensions", "write"),
            ("extensions", "delete"),
            ("trunks", "read"),
            ("trunks", "write"),
            ("routes", "read"),
            ("routes", "write"),
            ("queues", "read"),
            ("queues", "write"),
            ("queues", "realtime"),
            ("ivr", "read"),
            ("ivr", "write"),
            ("ivr", "publish"),
            ("cdr", "read"),
            ("cdr", "read:recording"),
            ("cdr", "delete"),
            ("cdr", "export"),
            ("calls", "read"),
            ("calls", "control"),
            ("ami", "access"),
            ("voicemail", "read"),
            ("voicemail", "write"),
            ("voicemail", "settings"),
            ("endpoints", "read"),
            ("endpoints", "write"),
            ("endpoints", "reboot"),
            ("wholesale", "read"),
            ("wholesale", "write"),
            ("wholesale", "billing"),
            ("metrics", "read"),
            ("diagnostics", "read"),
        ],
    ),
    (
        "supervisor",
        &[
            ("extensions", "read"),
            ("queues", "read"),
            ("queues", "write"),
            ("queues", "realtime"),
            ("cdr", "read"),
            ("cdr", "read:recording"),
            ("cdr", "export"),
            ("calls", "read"),
            ("calls", "control"),
            ("voicemail", "read"),
        ],
    ),
    (
        "operator",
        &[
            ("extensions", "read"),
            ("queues", "read"),
            ("queues", "realtime"),
            ("calls", "read"),
            ("calls", "control"),
        ],
    ),
    (
        "viewer",
        &[
            ("extensions", "read"),
            ("trunks", "read"),
            ("routes", "read"),
            ("queues", "read"),
            ("ivr", "read"),
            ("cdr", "read"),
            ("cdr", "export"),
            ("voicemail", "read"),
            ("endpoints", "read"),
            ("wholesale", "read"),
        ],
    ),
    ("ivr_editor", &[("ivr", "read"), ("ivr", "write")]),
    (
        "wholesale_admin",
        &[
            ("wholesale", "read"),
            ("wholesale", "write"),
            ("wholesale", "billing"),
            ("wholesale", "settings"),
        ],
    ),
];

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
                    .col(
                        ColumnDef::new(Column::Name)
                            .string()
                            .char_len(100)
                            .unique_key()
                            .not_null(),
                    )
                    .col(string(Column::Description).char_len(500))
                    .col(boolean(Column::IsSystem).default(false))
                    .col(timestamp(Column::CreatedAt).default(Expr::current_timestamp()))
                    .col(timestamp(Column::UpdatedAt).default(Expr::current_timestamp()))
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(role_permission::Entity)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(role_permission::Column::Id)
                            .big_integer()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(big_integer(role_permission::Column::RoleId))
                    .col(string(role_permission::Column::Resource).char_len(100))
                    .col(string(role_permission::Column::Action).char_len(100))
                    .foreign_key(
                        ForeignKey::create()
                            .from(role_permission::Entity, role_permission::Column::RoleId)
                            .to(Entity, Column::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(user_role::Entity)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(user_role::Column::Id)
                            .big_integer()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(big_integer(user_role::Column::UserId))
                    .col(big_integer(user_role::Column::RoleId))
                    .col(timestamp(user_role::Column::CreatedAt).default(Expr::current_timestamp()))
                    .foreign_key(
                        ForeignKey::create()
                            .from(user_role::Entity, user_role::Column::RoleId)
                            .to(Entity, Column::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        let db = manager.get_connection();
        let now = chrono::Utc::now();

        for (name, description) in SYSTEM_ROLES {
            let role = ActiveModel {
                name: Set(ToString::to_string(name)),
                description: Set(ToString::to_string(description)),
                is_system: Set(true),
                created_at: Set(now),
                updated_at: Set(now),
                ..Default::default()
            };
            let inserted = Entity::insert(role)
                .exec_with_returning(db)
                .await
                .map_err(|e| DbErr::Custom(format!("failed to seed role {}: {}", name, e)))?;

            for (resource, action) in ROLE_PERMISSIONS
                .iter()
                .find(|(rname, _)| *rname == *name)
                .map(|(_, perms)| *perms)
                .unwrap_or(&[])
            {
                let perm = role_permission::ActiveModel {
                    role_id: Set(inserted.id),
                    resource: Set(ToString::to_string(resource)),
                    action: Set(ToString::to_string(action)),
                    ..Default::default()
                };
                role_permission::Entity::insert(perm)
                    .exec(db)
                    .await
                    .map_err(|e| {
                        DbErr::Custom(format!(
                            "failed to seed permission {}:{} for role {}: {}",
                            resource, action, name, e
                        ))
                    })?;
            }
        }

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(user_role::Entity).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(role_permission::Entity).to_owned())
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
    use sea_orm::{Database, EntityTrait};
    use sea_orm_migration::MigratorTrait;

    async fn setup_db() -> sea_orm::DatabaseConnection {
        let db = Database::connect("sqlite::memory:")
            .await
            .expect("connect sqlite memory");
        Migrator::up(&db, None).await.expect("run migrations");
        db
    }

    #[tokio::test]
    async fn roles_are_seeded() {
        let db = setup_db().await;
        let roles = Entity::find().all(&db).await.expect("query roles");
        assert_eq!(roles.len(), SYSTEM_ROLES.len());
        let names: Vec<&str> = roles.iter().map(|r| r.name.as_str()).collect();
        for (name, _) in SYSTEM_ROLES {
            assert!(names.contains(name), "role {} not found", name);
        }
    }

    #[tokio::test]
    async fn roles_are_system() {
        let db = setup_db().await;
        let roles = Entity::find().all(&db).await.expect("query roles");
        for role in &roles {
            assert!(role.is_system, "role {} should be system", role.name);
        }
    }

    #[tokio::test]
    async fn superadmin_has_ami_access() {
        let db = setup_db().await;
        let roles = Entity::find().all(&db).await.expect("query roles");
        let superadmin = roles
            .iter()
            .find(|r| r.name == "superadmin")
            .expect("superadmin");
        let perms = role_permission::Entity::find()
            .filter(role_permission::Column::RoleId.eq(superadmin.id))
            .all(&db)
            .await
            .expect("query perms");
        let has_ami = perms
            .iter()
            .any(|p| p.resource == "ami" && p.action == "access");
        assert!(has_ami, "superadmin should have ami:access");
    }

    #[tokio::test]
    async fn viewer_has_no_write_permissions() {
        let db = setup_db().await;
        let roles = Entity::find().all(&db).await.expect("query roles");
        let viewer = roles.iter().find(|r| r.name == "viewer").expect("viewer");
        let perms = role_permission::Entity::find()
            .filter(role_permission::Column::RoleId.eq(viewer.id))
            .all(&db)
            .await
            .expect("query perms");
        let has_write = perms
            .iter()
            .any(|p| p.action == "write" || p.action == "delete" || p.action == "manage");
        assert!(
            !has_write,
            "viewer should not have write/delete/manage permissions"
        );
    }

    #[tokio::test]
    async fn admin_has_no_system_write() {
        let db = setup_db().await;
        let roles = Entity::find().all(&db).await.expect("query roles");
        let admin = roles.iter().find(|r| r.name == "admin").expect("admin");
        let perms = role_permission::Entity::find()
            .filter(role_permission::Column::RoleId.eq(admin.id))
            .all(&db)
            .await
            .expect("query perms");
        let has_system_write = perms
            .iter()
            .any(|p| p.resource == "system" && p.action == "write");
        assert!(!has_system_write, "admin should not have system:write");
    }

    #[tokio::test]
    async fn user_role_assignment() {
        let db = setup_db().await;
        let roles = Entity::find().all(&db).await.expect("query roles");
        let admin = roles.iter().find(|r| r.name == "admin").expect("admin");

        let assignment = user_role::ActiveModel {
            user_id: Set(42),
            role_id: Set(admin.id),
            created_at: Set(chrono::Utc::now()),
            ..Default::default()
        };
        user_role::Entity::insert(assignment)
            .exec(&db)
            .await
            .expect("insert user role");

        let found = user_role::Entity::find()
            .filter(user_role::Column::UserId.eq(42i64))
            .all(&db)
            .await
            .expect("query user roles");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].role_id, admin.id);
    }
}
