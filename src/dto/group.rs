#[derive(serde::Serialize, serde::Deserialize)]
pub struct Group {
    pub id: uuid::Uuid,
    pub name: String,
}
