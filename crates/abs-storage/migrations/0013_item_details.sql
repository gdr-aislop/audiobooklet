-- What the library list and the item request already return for every book, kept instead of
-- thrown away: facts for Book Details (subtitle, year, publisher, language, ISBN/ASIN, chapter
-- count, tags), the book's total size (a download estimate before its files are listed), whether
-- it has a cover on the server (no request for one that doesn't), and the server's own sort keys
-- ("Hobbit, The" / "Tolkien, J.R.R."). All nullable and filled by the next sync; NULL means
-- "unknown", which every reader treats exactly as before this migration.
ALTER TABLE items ADD COLUMN subtitle TEXT;
ALTER TABLE items ADD COLUMN size_bytes INTEGER;
ALTER TABLE items ADD COLUMN num_chapters INTEGER;
ALTER TABLE items ADD COLUMN tags TEXT NOT NULL DEFAULT '[]';
ALTER TABLE items ADD COLUMN published_year TEXT;
ALTER TABLE items ADD COLUMN publisher TEXT;
ALTER TABLE items ADD COLUMN language TEXT;
ALTER TABLE items ADD COLUMN explicit INTEGER CHECK (explicit IN (0, 1));
ALTER TABLE items ADD COLUMN isbn TEXT;
ALTER TABLE items ADD COLUMN asin TEXT;
ALTER TABLE items ADD COLUMN has_cover INTEGER CHECK (has_cover IN (0, 1));
ALTER TABLE items ADD COLUMN title_sort TEXT;
ALTER TABLE items ADD COLUMN author_sort TEXT;

-- Audio file facts from the item request, for diagnosing playback and download problems.
ALTER TABLE tracks ADD COLUMN codec TEXT;
ALTER TABLE tracks ADD COLUMN mime_type TEXT;
ALTER TABLE tracks ADD COLUMN bit_rate INTEGER;
ALTER TABLE tracks ADD COLUMN channels INTEGER;
