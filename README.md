# `repost-me-not`

> "I saw that already, it's a repost. Why didn't you see them post this earlier?"

> "I didn't scroll up and look, lmao"

Do you get tired of hearing that same exchange all week in your server? Fear not anymore.

`repost-me-not` is a simple Discord bot that does one thing: Looks for duplicate and similar images being posted in a server and ~~shames~~ calls the reposter.

## Install and setup
1. Make sure you have [Rust](https://rustup.rs/) installed.
2. Checkout this repo: `git clone https://github.com/BlackHoleFox/repost-me-not.git`
3. Rename `.env.default` to `.env` and open it in ~~nano~~ your text editor of choice.
4. Make a [Discord bot token](https://discord.com/developers/applications) and add it into the file.
5. Run `cargo run --release`
6. ???
7. Profit


### Warnings
- Don't run a single instance of this bot across multiple guilds. Its designed for one guild and explosions / privacy leaks will occur if you do otherwise.
- Due to the way the image tracking system works, its entirely possible for the image comparision logic to get gamed given a malicious user. Its like a really bad neural net whos results are entirely dependent on the sum of all the inputs up until that point. Tl;dr don't use this in any critical contexts.

## License

This project is licensed under both the [MIT license] or [Apache License] at your choice.

[MIT license]: https://github.com/BlackHoleFox/repost-me-not/blob/master/LICENSE-MIT
[Apache License]: https://github.com/BlackHoleFox/repost-me-not/blob/master/LICENSE-APACHE