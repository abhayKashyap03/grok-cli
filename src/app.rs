pub enum Author {
    User,
    System
}

pub struct ChatMessage {
    pub author: Author,
    pub content: String,
}

pub struct App {
    pub input: String,
    pub messages: Vec<ChatMessage>,
    pub is_loading: bool,
    pub scroll: u16,
}

impl App {
    pub fn new() -> App {
        App {
            input: String::new(),
            messages: Vec::new(),
            is_loading: false,
            scroll: 0,
        }
    }

    pub fn submit_msg(&mut self) {
        if !self.input.is_empty() {
            self.messages.push(ChatMessage {
                author: Author::User,
                content: self.input.clone()
            });

            self.input.clear();
        }
    }

    pub fn scroll_up(&mut self) {
        if self.scroll > 0 {
            self.scroll -= 1;
        }
    }

    pub fn scroll_down(&mut self) {
        self.scroll += 1;
    }

    // pub fn scroll_to_bottom(&mut self) {
    //     self.scroll += self.messages.len() as u16; 
    // }
}