use std::collections::HashSet;
use std::sync::Arc;

use gpui::{Context, Entity, EventEmitter, Task};
use music::{Album, Artist, ArtistCatalogue, Track};
use tokio::task::AbortHandle;

use crate::{Io, Library, LibraryEvent, Session, SessionEvent, Shelf, join};

/// Signals that a local artist no longer has a page after the library changes.
pub enum ArtistDetailEvent {
    Gone(String),
}

pub struct ArtistDetail {
    id: Option<String>,
    artist: Option<Arc<Artist>>,
    appears_on: Vec<Album>,
    loading: bool,
    error: Option<String>,
    is_library: bool,
    catalog_id: Option<String>,
    session: Entity<Session>,
    library: Entity<Library>,
    io: Io,
    task: Option<Task<()>>,
    request: Option<AbortHandle>,
    filling: bool,
    fill: Option<Task<()>>,
    filling_request: Option<AbortHandle>,
}

impl EventEmitter<ArtistDetailEvent> for ArtistDetail {}

impl ArtistDetail {
    pub fn new(
        session: Entity<Session>,
        library: Entity<Library>,
        io: Io,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(&session, |this, _, event, cx| match event {
            SessionEvent::SignedIn => {
                if let Some(id) = this.id.clone().filter(|id| !music::is_local_id(id)) {
                    this.clear();
                    this.open(&id, cx);
                }
            }
            SessionEvent::SignedOut => {
                if !this.id.as_deref().is_some_and(music::is_local_id) {
                    this.clear();
                    cx.notify();
                }
            }
            SessionEvent::Reconnected | SessionEvent::WentOffline => {}
            SessionEvent::LocalChanged => {
                if let Some(id) = this.id.clone().filter(|id| music::is_local_id(id)) {
                    this.clear();
                    this.open(&id, cx);
                }
            }
        })
        .detach();

        cx.subscribe(&library, |this, _, event, cx| {
            let LibraryEvent::TracksHidden(ids) = event else {
                return;
            };
            let Some(artist) = this.artist.as_mut() else {
                return;
            };
            let before = artist.top_tracks.len();
            Arc::make_mut(artist)
                .top_tracks
                .retain(|track| !track.id.as_ref().is_some_and(|id| ids.contains(id)));
            if artist.top_tracks.len() != before {
                cx.notify();
            }
        })
        .detach();

        cx.observe(&library, |this, _, cx| {
            if this.is_library
                && let Some(id) = this.id.clone()
            {
                this.load_library(&id, cx);
            }
        })
        .detach();

        Self {
            id: None,
            artist: None,
            appears_on: Vec::new(),
            loading: false,
            error: None,
            is_library: false,
            catalog_id: None,
            session,
            library,
            io,
            task: None,
            request: None,
            filling: false,
            fill: None,
            filling_request: None,
        }
    }

    pub fn artist(&self) -> Option<&Artist> {
        self.artist.as_deref()
    }

    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    pub fn is_library(&self) -> bool {
        self.is_library
    }

    pub fn catalog_id(&self) -> Option<&str> {
        self.catalog_id.as_deref()
    }

    pub fn tracks(&self) -> &[Track] {
        self.artist
            .as_ref()
            .map(|artist| artist.top_tracks.as_slice())
            .unwrap_or_default()
    }

    pub fn albums(&self) -> &[Album] {
        self.artist
            .as_ref()
            .map(|artist| artist.albums.as_slice())
            .unwrap_or_default()
    }

    /// What the artist guests on, filled in behind the overview.
    pub fn appears_on(&self) -> &[Album] {
        &self.appears_on
    }

    pub fn is_loading(&self) -> bool {
        self.loading
    }

    /// Whether the discography and the deeper popular tracks are still on their way, after
    /// the overview has already put the page up.
    pub fn is_filling(&self) -> bool {
        self.filling
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn open(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.id.as_deref() == Some(id)
            && !self.is_library
            && (self.loading || self.artist.is_some())
        {
            return;
        }

        self.clear();
        self.id = Some(id.to_owned());
        self.is_library = false;

        let Some(catalog) = self.session.read(cx).catalog(id) else {
            cx.notify();
            return;
        };
        if let Some(artist) = catalog.peek_artist(id) {
            self.artist = Some(artist);
            let id = id.to_owned();
            self.fill(&id, cx);
            cx.notify();
            return;
        }
        self.load(id.to_owned(), false, cx);
    }

    pub fn open_library(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.id.as_deref() == Some(id) && self.is_library && self.artist.is_some() {
            return;
        }

        self.clear();
        self.id = Some(id.to_owned());
        self.is_library = true;

        self.load_library(id, cx);
    }

    pub fn refresh_library(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.id.clone() else {
            return;
        };
        self.load_library(&id, cx);
    }

    fn load_library(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut name: Option<String> = None;
        let mut cover: Option<String> = None;

        for shelf in [Shelf::Streaming, Shelf::Local] {
            let held = self.library.read(cx);
            let state = held.state(shelf);
            if let Some(artist) = state.artists().iter().find(|a| a.id == id || a.name == id) {
                name = Some(artist.name.clone());
                cover = artist.cover.clone();
                break;
            }
        }

        if name.is_none() {
            for shelf in [Shelf::Streaming, Shelf::Local] {
                let held = self.library.read(cx);
                let state = held.state(shelf);
                if let Some(track) = state.tracks().iter().find(|t| {
                    t.artist_refs
                        .iter()
                        .any(|r| r.id.as_deref() == Some(id) || r.name == id)
                        || t.artists.eq_ignore_ascii_case(id)
                }) {
                    let matched_name = track
                        .artist_refs
                        .iter()
                        .find(|r| r.id.as_deref() == Some(id) || r.name == id)
                        .map(|r| r.name.clone())
                        .unwrap_or_else(|| track.artists.clone());
                    name = Some(matched_name);
                    cover = track.cover.clone();
                    break;
                }
            }
        }

        let artist_name = name.unwrap_or_else(|| id.to_owned());

        if !music::is_library_id(id) && !music::is_local_id(id) {
            self.catalog_id = Some(id.to_owned());
        } else {
            self.catalog_id = None;
        }

        let mut tracks = Vec::new();
        let mut seen_tracks = HashSet::new();
        for shelf in [Shelf::Streaming, Shelf::Local] {
            let held = self.library.read(cx);
            let state = held.state(shelf);
            for track in state.tracks() {
                let matches_artist = track.artist_refs.iter().any(|r| {
                    r.id.as_deref() == Some(id) || r.name.eq_ignore_ascii_case(&artist_name)
                }) || track.artists.eq_ignore_ascii_case(&artist_name);
                if matches_artist {
                    if let Some(track_id) = &track.id
                        && !seen_tracks.insert(track_id.clone())
                    {
                        continue;
                    }
                    tracks.push(track.clone());
                }
            }
        }

        let mut albums = Vec::new();
        let mut seen_albums = HashSet::new();
        for shelf in [Shelf::Streaming, Shelf::Local] {
            let held = self.library.read(cx);
            let state = held.state(shelf);
            for album in state.albums() {
                let matches_artist = album.artist_refs.iter().any(|r| {
                    r.id.as_deref() == Some(id) || r.name.eq_ignore_ascii_case(&artist_name)
                }) || album.artists.eq_ignore_ascii_case(&artist_name);
                if matches_artist && seen_albums.insert(album.id.clone()) {
                    albums.push(album.clone());
                }
            }
        }

        for track in &tracks {
            if let Some(album_id) = &track.album_id
                && seen_albums.insert(album_id.clone())
            {
                albums.push(Album {
                    id: album_id.clone(),
                    name: track.album.clone(),
                    artists: track.artists.clone(),
                    artist_refs: track.artist_refs.clone(),
                    cover: track.cover.clone(),
                    cover_large: track.cover.clone(),
                    release_type: music::ReleaseType::Album,
                    year: 0,
                    track_count: 0,
                    release_date: String::new(),
                    label: String::new(),
                    copyrights: Vec::new(),
                    added_at: track.added_at,
                });
            }
        }

        albums.sort_by(|a, b| {
            b.year
                .cmp(&a.year)
                .then_with(|| b.added_at.cmp(&a.added_at))
        });

        if cover.is_none() {
            cover = albums
                .iter()
                .find_map(|a| a.cover_large.clone().or_else(|| a.cover.clone()));
        }

        self.artist = Some(Arc::new(Artist {
            name: artist_name,
            cover_large: cover,
            biography: None,
            monthly_listeners: None,
            top_tracks: tracks,
            albums,
        }));
        self.loading = false;
        self.error = None;
        cx.notify();

        if let Some(catalog_id) = self.catalog_id.clone()
            && let Some(client) = self.session.read(cx).client()
        {
            let io = self.io.clone();
            let asked_id = catalog_id.clone();
            self.task = Some(cx.spawn(async move |this, cx| {
                let id = asked_id.clone();
                let found =
                    join(io.spawn(async move { client.artist_images(vec![id]).await })).await;
                if let Ok(found) = found
                    && let Some(url) = found.get(&asked_id)
                    && !url.is_empty()
                {
                    let url = url.clone();
                    this.update(cx, |this, cx| {
                        if let Some(artist) = this.artist.as_mut() {
                            Arc::make_mut(artist).cover_large = Some(url);
                            cx.notify();
                        }
                    })
                    .ok();
                }
            }));
        }
    }

    /// Drops the page and fetches it again from the provider, past the catalog cache, so it
    /// shows its loading state until the new overview lands.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.id.clone() else {
            return;
        };
        self.clear();
        self.id = Some(id.clone());
        self.load(id, true, cx);
    }

    /// Fetches the artist's overview and fills the rest of the page once it lands. A `fresh`
    /// load drops the cached artist first, so the provider is asked again.
    fn load(&mut self, id: String, fresh: bool, cx: &mut Context<Self>) {
        let Some(catalog) = self.session.read(cx).catalog(&id) else {
            cx.notify();
            return;
        };

        self.loading = true;
        cx.notify();

        let local = music::is_local_id(&id)
            .then(|| self.session.read(cx).local_client())
            .flatten();
        let request = self.io.spawn({
            let id = id.clone();
            async move {
                if let Some(client) = local
                    && !client
                        .all_artists()
                        .await?
                        .iter()
                        .any(|artist| artist.id == id)
                {
                    return Ok(None);
                }
                if fresh {
                    catalog.invalidate_artist(&id).await;
                }
                catalog.artist(&id).await.map(Some)
            }
        });
        self.request = Some(request.abort_handle());
        self.task = Some(cx.spawn(async move |this, cx| {
            let loaded = join(request).await;

            this.update(cx, |this, cx| {
                if this.id.as_deref() != Some(id.as_str()) {
                    return;
                }

                this.loading = false;
                this.request = None;
                match crate::settled(loaded, cx) {
                    Ok(Some(artist)) => {
                        this.artist = Some(artist);
                        this.fill(&id, cx);
                    }
                    Ok(None) => {
                        this.clear();
                        cx.emit(ArtistDetailEvent::Gone(id));
                    }
                    Err(reason) => this.error = Some(reason),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Asks the provider for the rest of the page once its overview is up: the whole
    /// discography and the popular tracks only the discography can rank. A provider that
    /// answers everything in `artist` has nothing to add here and the page stays as it is.
    fn fill(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(artist) = self.artist.clone() else {
            return;
        };
        let Some(catalog) = self.session.read(cx).catalog(id) else {
            return;
        };
        if let Some(catalogue) = catalog.peek_artist_catalogue(id) {
            self.absorb(&catalogue);
            return;
        }

        self.filling = true;
        let id = id.to_owned();
        let request = self.io.spawn({
            let id = id.clone();
            let known = artist.top_tracks.clone();
            async move { catalog.artist_catalogue(&id, &known).await }
        });
        self.filling_request = Some(request.abort_handle());
        self.fill = Some(cx.spawn(async move |this, cx| {
            let filled = join(request).await;

            this.update(cx, |this, cx| {
                if this.id.as_deref() != Some(id.as_str()) {
                    return;
                }

                this.filling = false;
                this.filling_request = None;
                match filled {
                    Ok(catalogue) => this.absorb(&catalogue),
                    Err(error) => log::warn!("artist: cannot fill the page: {error:#}"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Puts the catalogue over the overview. Every list replaces what the overview carried,
    /// and an empty one leaves that part of the page alone.
    fn absorb(&mut self, catalogue: &ArtistCatalogue) {
        let Some(artist) = self.artist.as_mut() else {
            return;
        };
        if catalogue.is_empty() {
            return;
        }
        let artist = Arc::make_mut(artist);
        if !catalogue.albums.is_empty() {
            artist.albums = catalogue.albums.clone();
        }
        if !catalogue.top_tracks.is_empty() {
            artist.top_tracks = catalogue.top_tracks.clone();
        }
        if !catalogue.appears_on.is_empty() {
            self.appears_on = catalogue.appears_on.clone();
        }
    }

    fn clear(&mut self) {
        self.task = None;
        self.fill = None;
        for request in [self.request.take(), self.filling_request.take()]
            .into_iter()
            .flatten()
        {
            request.abort();
        }
        self.id = None;
        self.artist = None;
        self.appears_on.clear();
        self.loading = false;
        self.filling = false;
        self.error = None;
        self.is_library = false;
        self.catalog_id = None;
    }
}
