-- Seed place_genre_aliases with a curated resolve map.
--
-- The table shipped empty in 0310 — with no rows, comparable-act counting is
-- a raw string compare: a room that billed "doom metal" and a tenant tagged
-- "doom" never see each other. This seed is the smallest curated map that
-- covers the scenes Virya actually books in; the MusicBrainz dump importer
-- supersedes it when it lands.
--
-- Conventions:
--   * `alias` is pre-normalized exactly as place_venue_key normalizes —
--     lowercase, trimmed, inner whitespace collapsed — and nothing else:
--     hyphens and punctuation stay literal, so "post-metal" and "post metal"
--     are different aliases and both must be listed.
--   * Canonicals are family-level, not taxonomic: comparability is a booking
--     question ("would a room that booked X book us"), so subgenre spellings
--     fold into the family a promoter actually treats as interchangeable.
--   * Self-mapping rows (alias = canonical) are listed deliberately: they
--     document the canonical spelling the family resolves to, and they make
--     the set inspectable without knowing which spellings are omitted.
--   * Deliberately distinct neighbors stay distinct: "jungle" is NOT
--     "drum and bass", "melodic hardcore" is not metalcore, and "screamo"
--     stands alone. Broadening a real genre boundary to buy a match is how
--     this map would lie.

INSERT INTO place_genre_aliases (alias, canonical) VALUES
    -- metalcore family (Virya's core)
    ('metalcore',                 'metalcore'),
    ('metal core',                'metalcore'),
    ('metallic hardcore',         'metalcore'),
    ('metallic hard core',        'metalcore'),
    ('hardcore metal',            'metalcore'),
    ('modern metalcore',          'metalcore'),
    ('melodic metalcore',         'metalcore'),
    ('progressive metalcore',     'metalcore'),
    ('symphonic metalcore',       'metalcore'),
    ('blackened metalcore',       'metalcore'),
    ('mathcore',                  'metalcore'),
    ('chaotic hardcore',          'metalcore'),
    ('nu-metalcore',              'metalcore'),
    ('nu metalcore',              'metalcore'),
    ('post-metalcore',            'metalcore'),
    ('post metalcore',            'metalcore'),
    ('easycore',                  'metalcore'),
    -- djent
    ('djent',                     'djent'),
    ('djent metal',               'djent'),
    ('djent instrumental',        'djent'),
    -- deathcore family
    ('deathcore',                 'deathcore'),
    ('death core',                'deathcore'),
    ('melodic deathcore',         'deathcore'),
    ('technical deathcore',       'deathcore'),
    ('blackened deathcore',       'deathcore'),
    ('slamcore',                  'deathcore'),
    -- modern metal umbrella
    ('modern metal',              'modern metal'),
    ('new metal',                 'modern metal'),
    ('alternative metal',         'modern metal'),
    ('alt metal',                 'modern metal'),
    ('alt-metal',                 'modern metal'),
    -- hardcore stays hardcore — adjacent to metalcore, not inside it
    ('hardcore',                  'hardcore'),
    ('hardcore punk',             'hardcore'),
    ('hardcore punk rock',        'hardcore'),
    ('hc',                        'hardcore'),
    ('beatdown',                  'hardcore'),
    ('beatdown hardcore',         'hardcore'),
    ('tough guy hardcore',        'hardcore'),
    -- melodic hardcore is its own family
    ('melodic hardcore',          'melodic hardcore'),
    ('melodic hc',                'melodic hardcore'),
    -- post-hardcore likewise
    ('post-hardcore',             'post-hardcore'),
    ('post hardcore',             'post-hardcore'),
    ('posthardcore',              'post-hardcore'),
    -- death metal family
    ('death metal',               'death metal'),
    ('deathmetal',                'death metal'),
    ('melodic death metal',       'death metal'),
    ('melodeath',                 'death metal'),
    ('technical death metal',     'death metal'),
    ('tech death',                'death metal'),
    ('brutal death metal',        'death metal'),
    ('old school death metal',    'death metal'),
    ('osdm',                      'death metal'),
    ('blackened death metal',     'death metal'),
    -- black metal family
    ('black metal',               'black metal'),
    ('blackmetal',                'black metal'),
    ('atmospheric black metal',   'black metal'),
    ('depressive black metal',    'black metal'),
    ('dsbm',                      'black metal'),
    ('post-black metal',          'black metal'),
    ('post black metal',          'black metal'),
    ('blackgaze',                 'black metal'),
    -- thrash
    ('thrash metal',              'thrash metal'),
    ('thrash',                    'thrash metal'),
    ('thrashmetal',               'thrash metal'),
    ('crossover thrash',          'thrash metal'),
    ('crossover',                 'thrash metal'),
    -- doom / sludge / stoner
    ('doom metal',                'doom metal'),
    ('doom',                      'doom metal'),
    ('doommetal',                 'doom metal'),
    ('stoner doom',               'doom metal'),
    ('epic doom',                 'doom metal'),
    ('funeral doom',              'doom metal'),
    ('drone metal',               'doom metal'),
    ('sludge metal',              'sludge metal'),
    ('sludge',                    'sludge metal'),
    ('sludge doom',               'sludge metal'),
    ('stoner metal',              'stoner metal'),
    ('stoner rock',               'stoner metal'),
    ('stoner',                    'stoner metal'),
    ('desert rock',               'stoner metal'),
    -- post-metal
    ('post-metal',                'post-metal'),
    ('post metal',                'post-metal'),
    ('postmetal',                 'post-metal'),
    ('atmospheric sludge',        'post-metal'),
    -- progressive
    ('progressive metal',         'progressive metal'),
    ('prog metal',                'progressive metal'),
    ('progmetal',                 'progressive metal'),
    ('technical metal',           'progressive metal'),
    ('tech metal',                'progressive metal'),
    ('progressive rock',          'progressive rock'),
    ('prog rock',                 'progressive rock'),
    ('progrock',                  'progressive rock'),
    -- nu metal / groove
    ('nu metal',                  'nu metal'),
    ('nu-metal',                  'nu metal'),
    ('numetal',                   'nu metal'),
    ('groove metal',              'groove metal'),
    -- heavy / traditional
    ('heavy metal',               'heavy metal'),
    ('metal',                     'heavy metal'),
    ('trad metal',                'heavy metal'),
    ('traditional metal',         'heavy metal'),
    ('nwobhm',                    'heavy metal'),
    -- power / symphonic / folk
    ('power metal',               'power metal'),
    ('powermetal',                'power metal'),
    ('symphonic metal',           'symphonic metal'),
    ('folk metal',                'folk metal'),
    ('pagan metal',               'folk metal'),
    ('viking metal',              'folk metal'),
    -- grind family
    ('grindcore',                 'grindcore'),
    ('grind',                     'grindcore'),
    ('powerviolence',             'grindcore'),
    ('deathgrind',                'grindcore'),
    ('goregrind',                 'grindcore'),
    -- punk neighbors
    ('screamo',                   'screamo'),
    ('skramz',                    'screamo'),
    ('emo',                       'emo'),
    ('pop punk',                  'pop punk'),
    ('pop-punk',                  'pop punk'),
    ('poppunk',                   'pop punk'),
    ('punk',                      'punk'),
    ('punk rock',                 'punk'),
    ('crust',                     'crust'),
    ('crust punk',                'crust'),
    ('d-beat',                    'd-beat'),
    -- electronic neighbors deliberately kept distinct (a drum & bass act is
    -- not a jungle act; neither is a metal comparable)
    ('drum and bass',             'drum and bass'),
    ('drum & bass',               'drum and bass'),
    ('dnb',                       'drum and bass'),
    ('drum n bass',               'drum and bass'),
    ('jungle',                    'jungle'),
    ('industrial metal',          'industrial metal'),
    ('industrial',                'industrial metal'),
    ('electronicore',             'metalcore')
ON CONFLICT (alias) DO NOTHING;
