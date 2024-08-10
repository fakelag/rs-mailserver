use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Email {
    pub mail_from: String,
    pub mail_to: String,
    pub mail_content: String,
}

impl Email {
    pub fn new() -> Email {
        Email {
            mail_from: "".to_string(),
            mail_to: "".to_string(),
            mail_content: "".to_string(),
        }
    }

    pub fn is_valid(&self) -> bool {
        return self.mail_content.len() > 0 && self.mail_from.len() > 0 && self.mail_to.len() > 0;
    }
}
