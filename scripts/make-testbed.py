"""Writes the eval testbed (#107): a small folder of notes, code, documents
and two pictures with known content, for `vinosearch eval` and
evals/queries.toml. Standard library only, and deterministic: the same bytes
on every run, so a change in the numbers is a change in vinoSearch.

    scripts/make-testbed.sh OUT

All text here is written for this testbed; the pictures are drawn below.
"""

import struct
import sys
import zipfile
import zlib
from pathlib import Path
from xml.sax.saxutils import escape

# ---------------------------------------------------------------- plain text

TEXT = {
    "notes/sourdough.md": """\
# Sourdough starter

Feed the starter twice a day while it lives on the counter: 50 g of flour,
50 g of water, 50 g of starter, the rest goes in the discard jar. Once it
doubles within six hours it is ready to bake with.

## Weekend loaf

- 500 g bread flour, 375 g water (75% hydration), 100 g active starter, 10 g salt
- Autolyse the flour and water for an hour before adding starter and salt
- Four sets of stretch and folds, thirty minutes apart
- Bulk ferment until the dough has grown by half, then shape and retard in the
  fridge overnight
- Bake in a preheated Dutch oven at 250 C, lid on for 20 minutes, lid off for 25

If the crumb is dense and gummy, the bulk ferment was too short.
""",
    "notes/bike-maintenance.md": """\
# Bike upkeep

Every two weeks: wipe the chain with a rag and add one drop of wet lube per
link, then wipe off the excess. Check tyre pressure: 80 psi front, 85 psi rear
on the road bike, 30 psi on the gravel tyres.

Every season: look at the brake pads; replace them when the grooves are gone.
Check the chain with the wear gauge, at 0.5% stretch it gets replaced before
it eats the cassette. True the rear wheel if it rubs the brake.

The bottom bracket started creaking in May. Cleaning and greasing the threads
fixed it for now.
""",
    "notes/meeting-2026-03-12.md": """\
# Product sync, 12 March

Attendees: Priya, Tomás, Wen, Daniel

## Decisions

- The mobile app launch moves from June to the third quarter. The offline sync
  isn't stable enough and nobody wants to ship it half-done.
- Priya owns QA for the release candidate and will set up the device lab.
- The pricing page redesign goes ahead without waiting for the app.

## Action items

- Tomás: write up the sync conflict cases we saw in the beta
- Wen: get two more Android phones for testing
- Daniel: tell the partners about the new date before the newsletter goes out
""",
    "notes/houseplants.md": """\
# Plants

The monstera wants water when the top two inches of soil are dry, roughly once
a week in summer and every two weeks in winter. It leans toward the window, so
turn the pot a quarter each Sunday.

The fiddle leaf fig dropped three leaves after the move. It hates draughts and
being moved, so it stays in the corner by the bookshelf now.

Yellow leaves with soggy soil mean too much water, not too little. Let the pot
drain fully and never leave it standing in the saucer. The snake plant can go
a month without water.
""",
    "notes/japan-trip.md": """\
# Japan, autumn

Day 1-3 Tokyo: Shimokitazawa, the Mori art museum, a day trip to Kamakura.
Day 4: Shinkansen to Kyoto, the 7-day JR pass covers it, reserve seats on the
left for the view of Mount Fuji.
Day 5-7 Kyoto: Fushimi Inari early in the morning before the crowds, the
Arashiyama bamboo grove, one night at a ryokan in Gion with a kaiseki dinner.
Day 8: Nara and the deer park, back to Osaka for the flight home.

Remember: carry cash, many small restaurants don't take cards. Get an IC card
(Suica) for the metro on the first day.
""",
    "notes/tax-2025.md": """\
# Taxes for 2025

Receipts for the home office are in the blue folder: desk, chair, a share of
the internet bill. Keep the charity donation receipts with them.

The accountant needs the year's T4 slips and the RRSP contribution receipts
before the end of March. Medical expenses only count above 3% of net income,
so the dental work probably doesn't help this year.
""",
    "notes/running-plan.txt": """\
Half marathon plan, twelve weeks

Weeks 1-4: three easy runs of 5 to 8 km, one long run growing by a kilometre
each week, starting at 10 km.
Weeks 5-8: add one interval session: 6 x 800 m at goal pace with 2 minutes of
jogging between. Long run up to 16 km.
Weeks 9-11: tempo run of 8 km at threshold, long run peaking at 19 km.
Week 12: taper. Two short easy runs and rest the two days before the race.

Race day: gels at 7 km and 14 km, don't start faster than 5:40 per km.
""",
    "projects/tidepool/README.md": """\
# tidepool

A command line tool that prints tide predictions for a harbour from the
harmonic constituents published by the national hydrographic service.

    tidepool --station 07795 --days 3

It reads the constituents (M2, S2, K1, O1 and the rest) once, caches them
under ~/.cache/tidepool, and sums the cosine terms for each minute to find
high and low water. Times are local, heights in metres above chart datum.

Accuracy is within 10 minutes and 15 cm of the official tables for the
stations we checked; storm surge is not modelled.
""",
    "projects/tidepool/src/ratelimit.rs": """\
//! Requests to the hydrographic service are limited to ten a second.

use std::time::Instant;

/// A token bucket: `capacity` tokens, refilled at `rate` per second.
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    rate: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(capacity: f64, rate: f64) -> Self {
        Self { capacity, tokens: capacity, rate, last: Instant::now() }
    }

    /// Take one token if there is one; false means the caller must wait.
    pub fn try_acquire(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}
""",
    "projects/tidepool/src/retry.go": """\
package fetch

import (
	"context"
	"time"
)

// RetryWithBackoff calls fn until it succeeds, waiting twice as long after
// each failure, starting at base and never longer than limit.
func RetryWithBackoff(ctx context.Context, attempts int, base, limit time.Duration, fn func() error) error {
	wait := base
	var err error
	for i := 0; i < attempts; i++ {
		if err = fn(); err == nil {
			return nil
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(wait):
		}
		wait *= 2
		if wait > limit {
			wait = limit
		}
	}
	return err
}
""",
    "code/levenshtein.py": """\
def edit_distance(a: str, b: str) -> int:
    \"\"\"The fewest single-character insertions, deletions and substitutions
    that turn a into b, keeping only two rows of the table.\"\"\"
    if len(a) < len(b):
        a, b = b, a
    previous = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        current = [i]
        for j, cb in enumerate(b, 1):
            current.append(min(
                previous[j] + 1,
                current[j - 1] + 1,
                previous[j - 1] + (ca != cb),
            ))
        previous = current
    return previous[-1]


def closest(word, vocabulary):
    \"\"\"The vocabulary entry needing the fewest edits, for spelling suggestions.\"\"\"
    return min(vocabulary, key=lambda v: edit_distance(word, v))
""",
    "code/debounce.ts": """\
/**
 * Wraps `fn` so that it runs once, `ms` milliseconds after the last call in a
 * burst. The search box uses it so typing doesn't send a request per key.
 */
export function debounce<A extends unknown[]>(fn: (...args: A) => void, ms: number) {
  let timer: ReturnType<typeof setTimeout> | undefined;
  return (...args: A) => {
    if (timer !== undefined) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = undefined;
      fn(...args);
    }, ms);
  };
}
""",
    "code/schema.sql": """\
-- Invoicing for the bakery's wholesale customers.

CREATE TABLE customers (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    vat_number  TEXT UNIQUE
);

CREATE TABLE invoices (
    id           INTEGER PRIMARY KEY,
    customer_id  INTEGER NOT NULL REFERENCES customers(id),
    issued_on    DATE NOT NULL,
    due_on       DATE NOT NULL,
    total_cents  INTEGER NOT NULL CHECK (total_cents >= 0),
    paid_on      DATE
);

-- Overdue invoices, oldest first.
CREATE VIEW overdue AS
    SELECT * FROM invoices
    WHERE paid_on IS NULL AND due_on < CURRENT_DATE
    ORDER BY due_on;
""",
    "code/backup.sh": """\
#!/bin/sh
# Nightly backup of the home folder to the NAS, run from cron at 02:30.
# Keeps hard-linked snapshots so unchanged files take no extra space.
set -eu

BACKUP_TARGET="nas.local:/volume1/backups/laptop"
today=$(date +%F)

rsync -a --delete \\
    --exclude '.cache' --exclude 'Downloads' \\
    --link-dest="../latest" \\
    "$HOME/" "$BACKUP_TARGET/$today/"
ssh nas.local "ln -sfn $today /volume1/backups/laptop/latest"
""",
    "code/ini.c": """\
#include <ctype.h>
#include <string.h>

/* Split one line of an INI file into key and value, in place. Returns 1 for
 * a key = value line, 0 for a blank line, comment or [section] header. */
int parse_ini_line(char *line, char **key, char **value)
{
    char *eq;

    while (isspace((unsigned char)*line))
        line++;
    if (*line == '\\0' || *line == ';' || *line == '#' || *line == '[')
        return 0;
    eq = strchr(line, '=');
    if (eq == NULL)
        return 0;
    *eq = '\\0';
    *key = line;
    *value = eq + 1;
    while (isspace((unsigned char)**value))
        (*value)++;
    return 1;
}
""",
    "fr/recette-ratatouille.md": """\
# Ratatouille de grand-mère

Couper deux aubergines, trois courgettes, deux poivrons rouges et un oignon en
dés. Faire revenir chaque légume séparément dans l'huile d'olive, pour qu'il
garde sa texture, puis tout réunir dans une cocotte avec quatre tomates
pelées, trois gousses d'ail écrasées, du thym et une feuille de laurier.

Laisser mijoter à feu doux pendant une heure, à couvert. Saler à la fin. La
ratatouille est encore meilleure le lendemain, servie tiède avec du riz ou un
œuf au plat.
""",
    "fr/bail-appartement.txt": """\
Contrat de location, appartement au 3e étage, rue des Lilas

Le loyer mensuel est de 920 euros, charges comprises, payable le 5 de chaque
mois. Le dépôt de garantie correspond à un mois de loyer hors charges ; il est
rendu dans les deux mois suivant la remise des clés, déduction faite des
réparations éventuelles.

Le locataire peut résilier le bail à tout moment avec un préavis de trois mois,
réduit à un mois en cas de mutation professionnelle. Les animaux de compagnie
sont acceptés. L'entretien de la chaudière est à la charge du locataire.
""",
    "hi/swasthya.md": """\
# सेहत के लिए रोज़ की आदतें

दिन में कम से कम आठ गिलास पानी पिएँ, गर्मियों में और ज़्यादा। सुबह उठकर
पहले एक गिलास गुनगुना पानी पीना अच्छा रहता है।

रात को सात से आठ घंटे की नींद ज़रूरी है। सोने से एक घंटा पहले फ़ोन और
टीवी बंद कर दें। रोज़ आधा घंटा पैदल चलें, और खाने में हरी सब्ज़ियाँ और
दाल शामिल करें। चीनी और तले हुए खाने से बचें।
""",
    "hi/jaipur-yatra.txt": """\
जयपुर यात्रा

पहले दिन हम आमेर किले गए। सुबह जल्दी पहुँचे तो भीड़ कम थी। शाम को हवा
महल देखा, उसकी नौ सौ से ज़्यादा खिड़कियाँ हैं जिनसे रानियाँ बाहर का
नज़ारा देखती थीं।

दूसरे दिन जंतर मंतर गए, जहाँ पत्थर की बड़ी धूपघड़ी आज भी सही समय बताती
है। बाज़ार से नीली मिट्टी के बर्तन और बंधेज के दुपट्टे खरीदे। खाने में दाल
बाटी चूरमा सबसे अच्छा लगा।
""",
    # Near misses: files on the same topics as the ones queries expect, so
    # the right file has to win on what it says, not on its topic.
    "notes/baguette.md": """\
# Baguettes with commercial yeast

A same-day dough: 500 g flour, 350 g water, 7 g instant yeast, 10 g salt.
Knead for ten minutes until smooth, rise for an hour, divide into three and
shape into long batons. Proof on a floured couche for forty minutes.

Slash each one at a shallow angle and bake at 240 C with a tray of boiling
water on the bottom rack for steam. They go stale by the next morning, so
freeze what isn't eaten.
""",
    "notes/car-maintenance.md": """\
# Car

Oil change every 8,000 km with 0W-20 synthetic; the last one was in February
at 62,400 km. Tyre pressure is 35 psi cold, front and back, as the sticker on
the door frame says. Swap to the winter tyres when the overnight lows stay
under 7 degrees, usually early November.

The windshield washer pump is noisy but works. Inspection is due in June.
""",
    "notes/meeting-2026-02-03.md": """\
# Product sync, 3 February

Attendees: Priya, Wen, Daniel

## Decisions

- The web dashboard ships on schedule at the end of the month.
- We drop the Windows desktop client; usage is under one percent.
- Support moves to a shared inbox instead of the old ticket system.

## Action items

- Wen: write the migration notes for the ticket system
- Daniel: draft the end-of-life email for the Windows client
""",
    "notes/garden-journal.md": """\
# Garden journal

April: planted peas and spinach along the fence, onion sets in the raised
bed. Covered the seedlings at night when frost was forecast.

June: tomatoes staked, basil between them to keep pests off. The zucchini
took over the corner again. Slugs ate half the lettuce; beer traps helped.

September: pulled the beans, sowed garlic for next summer, spread compost
over the empty beds.
""",
    "notes/reading-list.md": """\
# Reading list

- The Overstory, Richard Powers: trees, activists, a long book that earns it
- Piranesi, Susanna Clarke: a house of endless halls and tides
- The Dispossessed, Ursula K. Le Guin: an anarchist moon and its rich planet
- Braiding Sweetgrass, Robin Wall Kimmerer: botany and Potawatomi teaching
- A Psalm for the Wild-Built, Becky Chambers: a monk and a robot, short and kind
""",
    "notes/apartment-move.md": """\
# Moving checklist

Book the elevator with the building manager two weeks ahead. Forward mail
at the post office, and update the address with the bank, the employer and
the health card. Cancel the old internet plan, but only after the new one is
installed: the contract has a 30 day notice period.

Pack a box of first-night things: sheets, kettle, chargers, toilet paper,
medicine. Photograph every room of the old place on the way out, for the
deposit.
""",
    "code/retry.py": """\
import time


def retry(fn, attempts=3, delay=1.0):
    \"\"\"Call fn up to `attempts` times, sleeping a fixed `delay` between
    tries, and re-raise the last error.\"\"\"
    for i in range(attempts):
        try:
            return fn()
        except Exception:
            if i == attempts - 1:
                raise
            time.sleep(delay)
""",
    "code/fizzbuzz.rs": """\
fn main() {
    for n in 1..=100 {
        match (n % 3, n % 5) {
            (0, 0) => println!("FizzBuzz"),
            (0, _) => println!("Fizz"),
            (_, 0) => println!("Buzz"),
            _ => println!("{n}"),
        }
    }
}
""",
    "code/slugify.js": """\
// Turns a post title into the part of its URL after /blog/: lower case,
// accents dropped, anything that isn't a letter or digit becomes a hyphen.
export function slugify(title) {
  return title
    .normalize('NFKD')
    .replace(/[\\u0300-\\u036f]/g, '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
}
""",
    "fr/soupe-poireaux.md": """\
# Soupe poireaux pommes de terre

Émincer trois poireaux et les faire suer dans du beurre avec un oignon.
Ajouter quatre pommes de terre en morceaux et un litre et demi de bouillon de
légumes. Cuire vingt-cinq minutes, mixer, poivrer, et servir avec un trait de
crème fraîche. Se congèle très bien.
""",
    "fr/compte-rendu-ag.txt": """\
Compte rendu de l'assemblée générale des copropriétaires

Les travaux de ravalement de la façade sont votés pour le printemps, pour un
montant de 84 000 euros réparti selon les tantièmes. Le changement de syndic
est reporté à l'an prochain. Un local à vélos sera aménagé dans l'ancienne
loge du gardien. Les copropriétaires sont priés de ne plus laisser de
poussettes dans le hall.
""",
    "hi/dal-recipe.md": """\
# अरहर की दाल

एक कटोरी अरहर की दाल को धोकर आधे घंटे भिगो दें। कुकर में तीन कप पानी,
हल्दी और नमक के साथ तीन सीटी तक पकाएँ। अलग पैन में घी गरम करें, उसमें
जीरा, हींग, लहसुन और सूखी लाल मिर्च का तड़का लगाएँ और दाल में मिला दें।
ऊपर से हरा धनिया डालकर चावल के साथ परोसें।
""",
    # A longer file in sections, so the right chunk has to win too.
    "notes/home-handbook.md": """\
# House handbook

Everything about the house that someone looking after it would need to
know, in one place.

## Water

The main shut-off valve is in the basement, behind the furnace, on the pipe
coming through the front wall. Turn it clockwise to close. The outdoor taps
have their own valves above the laundry sink; close them in October and open
the taps outside so the pipes don't burst when it freezes.

The water heater is a 50 gallon gas tank from 2019. If there's no hot water,
check that the pilot light is lit through the little window at the bottom.

## Electricity

The breaker panel is in the laundry room. Breaker 14 is the kitchen
counter outlets, which trip if the kettle and the toaster run together.
Breaker 22 is the garage. The outlet by the back door is GFCI protected:
press reset if it's dead after rain.

## Heating and cooling

The furnace filter is 16x25x1 and gets changed every three months; spares
are on the shelf next to it. The thermostat runs a schedule: 21 degrees from
6 to 22, 18 overnight. The heat pump handles cooling in summer and heating
down to minus 15, below which the gas furnace takes over by itself.

## Garbage and recycling

Garbage and green bin go out every Tuesday night. Recycling alternates with
yard waste, every other Tuesday; the city calendar is on the fridge.
Batteries and paint go to the depot on Kenilworth, not the bins.

## Wi-Fi and alarms

The network is called Larkspur and the password is on the card inside the
kitchen cupboard door. The smoke alarms are tested on the first of the month;
the one at the top of the stairs chirps when its battery is low and takes a
9 volt.

## Neighbours

Marguerite next door has a spare key and waters the plants when we're away.
The family across the street lends their ladder; we lend them the snow
blower.
""",
    "docs/inventory.csv": """\
sku,item,warehouse,quantity,reorder_at
WX-4471,stainless steel mixing bowl 5 L,Hamilton,140,40
WX-4472,silicone baking mat,Hamilton,85,30
WX-5310,cast iron Dutch oven 6 qt,Mississauga,22,10
WX-5311,banneton proofing basket,Mississauga,64,20
WX-6020,bench scraper,Hamilton,210,50
WX-6021,digital kitchen scale,Mississauga,37,15
""",
    "docs/trail-log.csv": """\
date,trail,distance_km,elevation_m,notes
2026-04-04,Bruce Trail Dundas Valley,12.4,310,muddy after the rain
2026-04-18,Rattlesnake Point loop,8.1,190,turkey vultures over the cliff
2026-05-02,Webster Falls and Tews Falls,9.6,240,both waterfalls running high
2026-05-23,Mount Nemo,6.3,150,saw a rat snake on the rocks
""",
}

# ------------------------------------------------------------------ documents

PDFS = {
    # Each inner list is a page; each string a line.
    "docs/quarterly-report.pdf": [
        [
            "Northwind Kitchenware - Q2 report",
            "Revenue for the quarter was 4.2 million dollars, up 18 percent",
            "on the same quarter last year. Wholesale grew fastest, driven by",
            "two new restaurant supply contracts in Ontario.",
        ],
        [
            "Customer churn",
            "Monthly churn among online subscribers rose from 2.1 to 3.4",
            "percent. Exit surveys blame late deliveries during the warehouse",
            "move in April. Shipping times are back to two days since May.",
        ],
        [
            "Hiring plan",
            "We will hire four people in Q3: two warehouse staff for the",
            "Mississauga site, a customer support lead, and a buyer for",
            "the cookware line. Interviews start in the second week of July.",
        ],
    ],
    "docs/q1-report.pdf": [
        [
            "Northwind Kitchenware - Q1 report",
            "Revenue for the quarter was 3.6 million dollars. Retail sales",
            "were flat; the new cast iron line sold out in three weeks.",
        ],
        [
            "Customer churn",
            "Monthly churn among online subscribers fell to 2.1 percent, the",
            "lowest since launch, after the loyalty discount started in January.",
        ],
        [
            "Hiring",
            "Hiring is frozen until the warehouse move is finished.",
        ],
    ],
    "docs/dishwasher-warranty.pdf": [
        [
            "Limited warranty - model DW-300 dishwasher",
            "Parts and labour are covered for two years from the date of",
            "purchase. The stainless steel tub is covered for ten years.",
            "The warranty does not cover damage from improper installation,",
            "hard water scale, or use with detergents not made for dishwashers.",
            "To make a claim, call the service line with your receipt and the",
            "serial number printed on the inside of the door.",
        ],
    ],
    "docs/assurance-habitation.pdf": [
        [
            "Assurance habitation - conditions particulières",
            "Le contrat couvre l'incendie, les dégâts des eaux, le vol avec",
            "effraction et la responsabilité civile de tous les occupants.",
            "En cas de sinistre, déclarez-le dans les cinq jours ouvrés.",
            "La franchise est de 150 euros par sinistre. Les objets de valeur",
            "au-delà de 3000 euros doivent être déclarés à part.",
        ],
    ],
}

DOCX = {
    "docs/onboarding.docx": [
        "Welcome to the team",
        "On your first day, IT gives you a laptop. Sign in with the temporary "
        "password from your welcome email and change it straight away.",
        "Install the VPN client from the company portal before working from "
        "home; nothing internal is reachable without it.",
        "Pick up your building badge at the front desk on the ground floor. "
        "Lost badges cost 25 dollars to replace.",
        "Lunch on Fridays is on the company. Your buddy for the first month "
        "will show you around and answer questions.",
    ],
    "docs/rasoi-niyam.docx": [
        "दफ़्तर की रसोई के नियम",
        "इस्तेमाल के बाद अपने बर्तन ख़ुद धोकर रखें। सिंक में गंदे बर्तन न छोड़ें।",
        "फ्रिज हर शुक्रवार शाम को खाली किया जाता है। अपने डिब्बे पर नाम और "
        "तारीख़ लिखें, नहीं तो उन्हें फेंक दिया जाएगा।",
        "माइक्रोवेव में मछली गरम न करें।",
    ],
}

PPTX = {
    "docs/npu-talk.pptx": [
        ["Search on the laptop's NPU", "Embedding files locally, without a GPU or the cloud"],
        [
            "Quantization",
            "Weights in INT8 with per-channel scales",
            "Four times smaller, and the NPU runs integer maths at low power",
        ],
        [
            "Benchmarks",
            "Query latency and energy per thousand chunks",
            "Measured on the same laptop, plugged in and on battery",
        ],
    ],
}

XLSX = {
    "docs/household-budget.xlsx": [
        ["category", "monthly", "notes"],
        ["rent", "1850", "due on the first"],
        ["groceries", "620", "farmers market on Saturdays"],
        ["electricity", "95", "higher in winter with the heat pump"],
        ["internet", "70", "fibre, contract ends in November"],
        ["car insurance", "142", "renews in August"],
        ["daycare", "980", "subsidised rate"],
    ],
}

RTF = {
    "docs/heating-complaint.rtf": [
        "Dear Ms. Okafor,",
        "I am writing again about the heating in apartment 4B. The radiators in "
        "the bedroom and living room have been cold since the 3rd of January, "
        "and the space heater you lent us trips the breaker.",
        "Under the lease the building has to keep the unit at 20 degrees. "
        "Please have the boiler looked at this week, or I will contact the "
        "city's property standards office.",
        "Sincerely, Jordan Reyes",
    ],
}

DOC = {
    "docs/garden-minutes.doc": [
        "Riverside Community Garden - annual meeting minutes",
        "Present: eleven members. The treasurer reported 2,340 dollars in the "
        "account after buying the new rain barrels.",
        "Plot fees stay at 40 dollars a season. The waiting list has nineteen "
        "names; two plots by the fence will be split in half to make room.",
        "The compost bins will be turned on the first Saturday of each month. "
        "Volunteers sign up on the shed door.",
    ],
}

# ---------------------------------------------------------------- the writers

FIXED = (1980, 1, 1, 0, 0, 0)


def write_zip(path, parts):
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        for name, data in parts:
            info = zipfile.ZipInfo(name, FIXED)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            z.writestr(info, data)


def content_types(overrides):
    items = "".join(
        f'<Override PartName="/{name}" ContentType="{ctype}"/>' for name, ctype in overrides
    )
    return (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
        '<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
        '<Default Extension="xml" ContentType="application/xml"/>'
        f"{items}</Types>"
    )


def rels(targets):
    items = "".join(
        f'<Relationship Id="rId{i}" Type="{rtype}" Target="{target}"/>'
        for i, (rtype, target) in enumerate(targets, 1)
    )
    return (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        f"{items}</Relationships>"
    )


OFFICE_DOC = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument"


def docx(path, paragraphs):
    body = "".join(f"<w:p><w:r><w:t>{escape(p)}</w:t></w:r></w:p>" for p in paragraphs)
    document = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
        f"<w:body>{body}</w:body></w:document>"
    )
    write_zip(path, [
        ("[Content_Types].xml", content_types([(
            "word/document.xml",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml",
        )])),
        ("_rels/.rels", rels([(OFFICE_DOC, "word/document.xml")])),
        ("word/document.xml", document),
    ])


def pptx(path, slides):
    p = "http://schemas.openxmlformats.org/presentationml/2006/main"
    a = "http://schemas.openxmlformats.org/drawingml/2006/main"
    r = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
    parts = []
    for n, lines in enumerate(slides, 1):
        paras = "".join(f"<a:p><a:r><a:t>{escape(line)}</a:t></a:r></a:p>" for line in lines)
        parts.append((
            f"ppt/slides/slide{n}.xml",
            '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
            f'<p:sld xmlns:p="{p}" xmlns:a="{a}" xmlns:r="{r}"><p:cSld><p:spTree>'
            '<p:sp><p:nvSpPr><p:cNvPr id="2" name="Text"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>'
            f"<p:spPr/><p:txBody><a:bodyPr/>{paras}</p:txBody></p:sp>"
            "</p:spTree></p:cSld></p:sld>",
        ))
    ids = "".join(f'<p:sldId id="{255 + n}" r:id="rId{n}"/>' for n in range(1, len(slides) + 1))
    presentation = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        f'<p:presentation xmlns:p="{p}" xmlns:r="{r}"><p:sldIdLst>{ids}</p:sldIdLst>'
        "</p:presentation>"
    )
    slide_type = "application/vnd.openxmlformats-officedocument.presentationml.slide+xml"
    write_zip(path, [
        ("[Content_Types].xml", content_types(
            [("ppt/presentation.xml",
              "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml")]
            + [(name, slide_type) for name, _ in parts]
        )),
        ("_rels/.rels", rels([(OFFICE_DOC, "ppt/presentation.xml")])),
        ("ppt/_rels/presentation.xml.rels", rels([
            ("http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide",
             f"slides/slide{n}.xml")
            for n in range(1, len(slides) + 1)
        ])),
        ("ppt/presentation.xml", presentation),
        *parts,
    ])


def xlsx(path, rows):
    def cell(ref, value):
        if value.isdigit():
            return f'<c r="{ref}"><v>{value}</v></c>'
        return f'<c r="{ref}" t="inlineStr"><is><t>{escape(value)}</t></is></c>'

    body = "".join(
        f'<row r="{i}">'
        + "".join(cell(f"{chr(65 + j)}{i}", v) for j, v in enumerate(row))
        + "</row>"
        for i, row in enumerate(rows, 1)
    )
    main = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
    r = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
    write_zip(path, [
        ("[Content_Types].xml", content_types([
            ("xl/workbook.xml",
             "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"),
            ("xl/worksheets/sheet1.xml",
             "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"),
        ])),
        ("_rels/.rels", rels([(OFFICE_DOC, "xl/workbook.xml")])),
        ("xl/_rels/workbook.xml.rels", rels([(
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet",
            "worksheets/sheet1.xml",
        )])),
        ("xl/workbook.xml",
         '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
         f'<workbook xmlns="{main}" xmlns:r="{r}"><sheets>'
         '<sheet name="Budget" sheetId="1" r:id="rId1"/></sheets></workbook>'),
        ("xl/worksheets/sheet1.xml",
         '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
         f'<worksheet xmlns="{main}"><sheetData>{body}</sheetData></worksheet>'),
    ])


def pdf(path, pages):
    """A PDF with a text layer in Helvetica, one content stream per page."""
    objects = []

    def add(body):
        objects.append(body)
        return len(objects)

    font = add(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica "
               b"/Encoding /WinAnsiEncoding >>")
    pages_id = len(objects) + 1 + 2 * len(pages)
    kids = []
    for lines in pages:
        ops = ["BT", "/F1 12 Tf", "14 TL", "72 720 Td"]
        for line in lines:
            text = line.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
            ops.append(f"({text}) Tj T*")
        ops.append("ET")
        stream = "\n".join(ops).encode("cp1252")
        content = add(b"<< /Length %d >>\nstream\n" % len(stream) + stream + b"\nendstream")
        kids.append(add(
            b"<< /Type /Page /Parent %d 0 R /MediaBox [0 0 612 792] "
            b"/Resources << /Font << /F1 %d 0 R >> >> /Contents %d 0 R >>"
            % (pages_id, font, content)
        ))
    assert add(
        b"<< /Type /Pages /Kids [%s] /Count %d >>"
        % (b" ".join(b"%d 0 R" % k for k in kids), len(kids))
    ) == pages_id
    catalog = add(b"<< /Type /Catalog /Pages %d 0 R >>" % pages_id)

    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for n, body in enumerate(objects, 1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % n + body + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objects) + 1)
    out += b"".join(b"%010d 00000 n \n" % o for o in offsets)
    out += b"trailer\n<< /Size %d /Root %d 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (
        len(objects) + 1, catalog, xref)
    path.write_bytes(bytes(out))


def rtf(path, paragraphs):
    def encode(text):
        out = []
        for ch in text:
            if ch in "\\{}":
                out.append("\\" + ch)
            elif ord(ch) < 128:
                out.append(ch)
            else:
                out.append(f"\\u{ord(ch)}?")
        return "".join(out)

    body = "\n".join(f"\\pard {encode(p)}\\par" for p in paragraphs)
    path.write_text(
        "{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Times New Roman;}}\n" + body + "\n}\n",
        encoding="ascii",
    )


SECTOR = 512
FREESECT, ENDOFCHAIN, FATSECT = 0xFFFFFFFF, 0xFFFFFFFE, 0xFFFFFFFD


def compound_file(streams):
    """A version 3 compound file holding `streams` (name, bytes) under the
    root, each padded to 4096 bytes so none goes in the mini stream."""
    data = []
    for name, body in streams:
        body = body + b"\0" * (max(4096, -(-len(body) // SECTOR) * SECTOR) - len(body))
        data.append((name, body))
    # Sectors: the directory (one sector holds four entries), then each stream,
    # then the FAT.
    entries = 1 + len(data)
    assert entries <= 4
    chains, next_free = [], 1
    for _, body in data:
        n = len(body) // SECTOR
        chains.append((next_free, n))
        next_free += n
    fat_at = next_free
    total = fat_at + 1
    assert total <= SECTOR // 4
    fat = [FREESECT] * (SECTOR // 4)
    fat[0] = ENDOFCHAIN
    for start, n in chains:
        for s in range(start, start + n - 1):
            fat[s] = s + 1
        fat[start + n - 1] = ENDOFCHAIN
    fat[fat_at] = FATSECT

    def entry(name, kind, start, size, left=FREESECT, right=FREESECT, child=FREESECT):
        raw = name.encode("utf-16-le") + b"\0\0"
        return (
            raw.ljust(64, b"\0")
            + struct.pack("<HBB", len(raw), kind, 1)  # black
            + struct.pack("<III", left, right, child)
            + b"\0" * 16 + struct.pack("<I", 0) + b"\0" * 16
            + struct.pack("<IQ", start, size)
        )

    # The children as a chain to the right, in the order cfb sorts names:
    # shorter first, then by upper-cased name.
    order = sorted(range(len(data)), key=lambda i: (len(data[i][0]), data[i][0].upper()))
    dir_ids = {i: n + 1 for n, i in enumerate(order)}
    directory = entry("Root Entry", 5, ENDOFCHAIN, 0, child=dir_ids[order[0]])
    for n, i in enumerate(order):
        right = dir_ids[order[n + 1]] if n + 1 < len(order) else FREESECT
        name, body = data[i]
        directory += entry(name, 2, chains[i][0], len(body), right=right)
    unused = b"\0" * 68 + struct.pack("<III", FREESECT, FREESECT, FREESECT) + b"\0" * 48
    directory += unused * (4 - entries)

    header = (
        bytes.fromhex("D0CF11E0A1B11AE1") + b"\0" * 16
        + struct.pack("<HHHHH", 0x3E, 3, 0xFFFE, 9, 6)
        + b"\0" * 6
        + struct.pack("<IIIIIIIII", 0, 1, 0, 0, 4096, ENDOFCHAIN, 0, ENDOFCHAIN, 0)
        + struct.pack("<I", fat_at) + struct.pack("<I", FREESECT) * 108
    )
    assert len(header) == SECTOR
    fat_bytes = struct.pack("<%dI" % len(fat), *fat)
    return header + directory + b"".join(body for _, body in data) + fat_bytes


def word97(path, paragraphs):
    """A Word 97 .doc: the FIB fields vinoSearch reads, one piece of 8-bit
    text, and the piece table in 1Table."""
    text = ("\r".join(paragraphs) + "\r").encode("cp1252")
    text_at = 1024
    word = bytearray(text_at + len(text))
    struct.pack_into("<HH", word, 0, 0xA5EC, 0xC1)
    struct.pack_into("<H", word, 0x0A, 0x0200)  # the table is 1Table
    struct.pack_into("<H", word, 32, 14)  # csw: FibRgW97 is 14 words
    rg_lw = 34 + 2 * 14
    struct.pack_into("<H", word, rg_lw, 22)  # cslw
    struct.pack_into("<I", word, rg_lw + 2 + 3 * 4, len(text))  # ccpText
    fc_lcb = rg_lw + 2 + 4 * 22
    struct.pack_into("<H", word, fc_lcb, 93)  # cbRgFcLcb
    word[text_at:] = text
    # Clx: a Pcdt of one piece; its fc is compressed (bit 30), as a byte
    # offset times two.
    plc = struct.pack("<II", 0, len(text)) + struct.pack("<HIH", 0, 0x40000000 | (text_at * 2), 0)
    table = b"\x02" + struct.pack("<I", len(plc)) + plc
    struct.pack_into("<II", word, fc_lcb + 2 + 33 * 8, 0, len(table))
    path.write_bytes(compound_file([("WordDocument", bytes(word)), ("1Table", table)]))


def png(path, width, height, pixel):
    rows = b"".join(
        b"\0" + b"".join(bytes(pixel(x, y)) for x in range(width)) for y in range(height)
    )

    def chunk(kind, body):
        return (struct.pack(">I", len(body)) + kind + body
                + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF))

    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(rows, 9))
        + chunk(b"IEND", b"")
    )


def red_ball(x, y):
    # A red disc lit from the top left, on a white table.
    dx, dy = x - 112, y - 112
    if dx * dx + dy * dy > 80 * 80:
        return (245, 245, 240)
    light = max(0.0, 1.0 - ((x - 85) ** 2 + (y - 85) ** 2) / (2 * 80 * 80))
    return (int(150 + 100 * light), int(20 + 60 * light), int(20 + 50 * light))


def sunset(x, y):
    # An orange sky over dark blue sea, the sun on the horizon.
    if y < 140:
        t = y / 140
        sky = (int(255 - 40 * t), int(120 + 70 * t), int(40 + 40 * t))
        if (x - 112) ** 2 + (y - 140) ** 2 < 30 ** 2:
            return (255, 230, 120)
        return sky
    t = (y - 140) / 84
    return (int(30 - 20 * t), int(40 - 20 * t), int(90 - 30 * t))


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: make-testbed.py OUT")
    out = Path(sys.argv[1])

    def dest(name):
        p = out / name
        p.parent.mkdir(parents=True, exist_ok=True)
        return p

    for name, body in TEXT.items():
        dest(name).write_text(body, encoding="utf-8")
    for name, pages in PDFS.items():
        pdf(dest(name), pages)
    for name, paras in DOCX.items():
        docx(dest(name), paras)
    for name, slides in PPTX.items():
        pptx(dest(name), slides)
    for name, rows in XLSX.items():
        xlsx(dest(name), rows)
    for name, paras in RTF.items():
        rtf(dest(name), paras)
    for name, paras in DOC.items():
        word97(dest(name), paras)
    png(dest("photos/red-ball.png"), 224, 224, red_ball)
    png(dest("photos/sunset.png"), 224, 224, sunset)


main()
