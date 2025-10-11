use sea_orm_migration::{MigrationTrait, MigratorTrait};
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(super::user::Migration),
            Box::new(super::department::Migration),
            Box::new(super::bill_template::Migration),
            Box::new(super::extension::Migration),
            Box::new(super::sip_trunk::Migration),
            Box::new(super::routing::Migration),
            Box::new(super::call_record::Migration),
        ]
    }
}
