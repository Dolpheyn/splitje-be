pub fn to_sqlx_uuid(id: uuid::Uuid) -> sqlx::types::Uuid {
    sqlx::types::Uuid::from_bytes(*id.as_bytes())
}

pub fn to_uuid(id: sqlx::types::Uuid) -> uuid::Uuid {
    uuid::Uuid::from_bytes(*id.as_bytes())
}
