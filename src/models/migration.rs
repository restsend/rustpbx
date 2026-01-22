use sea_orm_migration::{MigrationTrait, MigratorTrait};
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(super::user::Migration),
            Box::new(super::department::Migration),
            Box::new(super::extension::Migration),
            Box::new(super::extension_department::Migration),
            Box::new(super::sip_trunk::Migration),
            Box::new(super::presence::Migration),
            Box::new(super::routing::Migration),
            Box::new(crate::addons::queue::models::Migration),
            Box::new(super::call_record::Migration),
            Box::new(super::frequency_limit::Migration),
            Box::new(super::call_record_indices::Migration),
            Box::new(super::call_record_optimization_indices::Migration),
            Box::new(super::call_record_dashboard_index::Migration),
            Box::new(super::call_record_from_number_index::Migration),
            Box::new(super::add_rewrite_columns::Migration),
        ]
    }
}
