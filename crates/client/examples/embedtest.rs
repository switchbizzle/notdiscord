//! Dev harness: does an <iframe> to each embed host actually render inside
//! the dioxus/WebView2 page? Renders three test iframes, then screenshots its
//! OWN window (windows-capture) so the result can be inspected headlessly.
//! Run with `cargo run -p client --example embedtest`, wait ~12s, then look at
//! embedtest-shot.png in the repo root.

use dioxus::prelude::*;

fn main() {
    // Self-screenshot after the widgets have had time to load.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(10));
        match shoot() {
            Ok(()) => println!("saved embedtest-shot.png"),
            Err(e) => println!("screenshot failed: {e}"),
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
        std::process::exit(0);
    });
    dioxus::LaunchBuilder::desktop().launch(App);
}

fn shoot() -> Result<(), String> {
    use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
    use windows_capture::encoder::ImageFormat;
    use windows_capture::frame::Frame;
    use windows_capture::graphics_capture_api::InternalCaptureControl;
    use windows_capture::settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    };
    use windows_capture::window::Window;

    struct Snap;
    impl GraphicsCaptureApiHandler for Snap {
        type Flags = ();
        type Error = Box<dyn std::error::Error + Send + Sync>;
        fn new(_: Context<Self::Flags>) -> Result<Self, Self::Error> {
            Ok(Self)
        }
        fn on_frame_arrived(
            &mut self,
            frame: &mut Frame,
            control: InternalCaptureControl,
        ) -> Result<(), Self::Error> {
            frame.save_as_image("embedtest-shot.png", ImageFormat::Png)?;
            control.stop();
            Ok(())
        }
    }

    let window = Window::from_contains_name("Dioxus App").map_err(|e| e.to_string())?;
    let settings = Settings::new(
        window,
        CursorCaptureSettings::WithoutCursor,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Rgba8,
        (),
    );
    let control = Snap::start_free_threaded(settings).map_err(|e| e.to_string())?;
    control.wait().map_err(|e| e.to_string())
}

#[component]
fn App() -> Element {
    // Mirror LinkCard exactly: iframe appears only after a click swaps it in.
    let mut playing = use_signal(|| false);

    // Simulate the user's click on the play button after the page settles.
    use_future(move || async move {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        dioxus::document::eval("document.querySelector('.link-card-play').click()");
    });

    rsx! {
        div { style: "background:#313338;color:#fff;font-family:sans-serif;padding:12px;height:100vh;overflow:auto",
            h3 { if playing() { "clicked -> iframe swapped in" } else { "waiting for simulated click..." } }
            div { class: "link-card", style: "max-width:440px;border-left:4px solid #5865f2;background:#2b2d31",
                if playing() {
                    iframe {
                        class: "link-card-embed",
                        src: "https://w.soundcloud.com/player/?url=https%3A%2F%2Fsoundcloud.com%2Fphilosrecords%2Fdaily-bread-stormy-seas-we-are&color=%23ff5500&auto_play=false&hide_related=true&show_comments=false&show_teaser=false&visual=false",
                        style: "height: 166px; width:100%; border:0; background:#000; display:block",
                        allow: "autoplay; encrypted-media; clipboard-write; picture-in-picture; fullscreen",
                    }
                } else {
                    div { style: "position:relative;line-height:0",
                        div { style: "width:100%;height:166px;background:#555" }
                        button {
                            class: "link-card-play",
                            style: "position:absolute;inset:0;margin:auto;width:56px;height:56px",
                            onclick: move |_| playing.set(true),
                            "PLAY"
                        }
                    }
                }
            }
        }
    }
}
