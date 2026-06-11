//! Fetch a few real feeds and dump the media data feedr would surface.
//! Run with: cargo run --example dump_media

use feedr::feed::{Feed, FeedFetchResult};

fn main() {
    let urls = [
        "https://www.youtube.com/feeds/videos.xml?channel_id=UCsBjURrPoezykLs9EqgamOA",
        "https://xkcd.com/atom.xml",
        "https://www.smbc-comics.com/comic/rss",
    ];

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();

    for url in urls {
        let t0 = std::time::Instant::now();
        println!("\n=== {url} ===");
        match Feed::fetch_url(url, &client, None, None) {
            Ok(FeedFetchResult::Feed(feed)) => {
                println!("[feed] {} ({} items)", feed.title, feed.items.len());
                for item in feed.items.iter().take(3) {
                    println!("  - {}", item.title);
                    println!("      link      = {:?}", item.link);
                    println!("      thumbnail = {:?}", item.thumbnail);
                    for m in &item.media {
                        println!(
                            "      media     = url={} kind={:?} mime={:?} {}x{} dur={:?}s size={:?}",
                            m.url, m.kind, m.mime,
                            m.width.unwrap_or(0), m.height.unwrap_or(0),
                            m.duration_secs, m.size_bytes,
                        );
                    }
                    if let Some(primary) = item.primary_media() {
                        println!("      primary   = {} ({:?})", primary.url, primary.kind);
                    }
                }
            }
            Ok(FeedFetchResult::DiscoveredFeeds { .. }) => {
                println!("  (discovered feeds, not a direct feed URL)");
            }
            Err(e) => println!("  ERROR: {e}"),
        }
        println!("  (fetch took {:.1}s)", t0.elapsed().as_secs_f32());
    }
}
