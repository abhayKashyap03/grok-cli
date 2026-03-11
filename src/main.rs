mod app;
mod grok;

use ratatui::{
    Terminal, backend::CrosstermBackend, layout::{Constraint, Direction, Layout}, style::{Color, Style}, text::{Span, Text}, widgets::{Block, Borders, Paragraph, Wrap}
};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{io};
use crate::app::Author;
use tokio::sync::mpsc;

use app::App;
use grok::{GrokClient, Message as GrokMessage};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    
    println!("Initializing CLI...");

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    let (tx, mut rx) = mpsc::channel::<String>(100);

    dotenvy::dotenv().ok();
    let grok_client = GrokClient::new();

    loop {
        if let Ok(response) = rx.try_recv() {
            app.messages.push(app::ChatMessage {
                author: app::Author::System,
                content: response
            });
            app.is_loading = false;
            // app.scroll_to_bottom();
        }

        terminal.draw(|f| {
            let chunks = Layout::default()
                            .direction(Direction::Vertical)
                            .margin(1)
                            .constraints([Constraint::Min(3), Constraint::Length(6)].as_ref())
                            .split(f.area());

            let title = if app.is_loading { " Grok is thinking... " } else { " Conversation " };

            let mut chat_content = Text::default();

            for m in &app.messages {
                let (name, color) = match m.author {
                    Author::System => ("Grok: ", Color::Yellow),
                    Author::User => ("You: ", Color::Cyan)
                };

                chat_content.extend(Text::from(Span::styled(format!("{} ", name), Style::default().fg(color).bold())));
                chat_content.extend(Text::from(m.content.as_str()));
                chat_content.extend(Text::from("")); // empty line between message
            }
            
            let chat_widget = Paragraph::new(chat_content)
                                        .block(Block::default().title(title).borders(Borders::ALL))
                                        .wrap(Wrap { trim: true })
                                        .scroll((app.scroll, 0));
            f.render_widget(chat_widget, chunks[0]);

            let input_content = Paragraph::new(app.input.as_str())
                                                    .style(Style::default().fg(Color::Cyan))
                                                    .block(Block::default().title(" Message Grok ")
                                                    .borders(Borders::ALL))
                                                    .wrap(Wrap { trim: true })
                                                    .scroll((app.scroll, 0));
            f.render_widget(input_content, chunks[1]);
        })?;

        if let Event::Key(key) =  event::read()? {
            match key.code {
                KeyCode::Esc => {
                    break;
                }
                KeyCode::Up => {
                    app.scroll_up();
                }
                KeyCode::Down => {
                    app.scroll_down();
                }
                KeyCode::Char(c) => {
                    app.input.push(c);
                }
                KeyCode::Backspace => {
                    app.input.pop();
                }
                KeyCode::Enter => {
                    if !app.is_loading {    
                        app.submit_msg();
                        // app.scroll_to_bottom();
                        app.is_loading = true;

                        let mut history = Vec::new();
                        for m in &app.messages {
                            history.push(GrokMessage {
                                role: match m.author {
                                    app::Author::User => "user".to_string(),
                                    app::Author::System => "system".to_string(),
                                },
                                content: m.content.clone(),
                            });
                        }

                        let tx_clone = tx.clone();
                        let client_clone = grok_client.clone();

                        tokio::spawn(async move {
                            match client_clone.send_chat(history).await {
                                Ok(response) => {
                                    let _ = tx_clone.send(response).await;
                                }
                                Err(e) => {
                                    let _ = tx_clone.send(format!("Error: {}", e)).await;
                                }
                            }
                        });
                    }
                }
                _ => {}
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;

    Ok(())
}
