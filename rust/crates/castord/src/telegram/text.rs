//! What the bot says, in the four portal languages (telegram.ts).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    De,
    Fr,
    It,
}

/// The first of the candidates that is a supported language, else German.
pub fn lang(candidates: &[Option<&str>]) -> Lang {
    candidates
        .iter()
        .flatten()
        .find_map(|c| match c.get(..2) {
            Some("en") => Some(Lang::En),
            Some("de") => Some(Lang::De),
            Some("fr") => Some(Lang::Fr),
            Some("it") => Some(Lang::It),
            _ => None,
        })
        .unwrap_or(Lang::De)
}

pub struct Text {
    pub welcome: &'static str,
    pub paired: &'static str,
    pub bad_code: &'static str,
    pub new_chat: &'static str,
    pub disconnected: &'static str,
    pub unavailable: &'static str,
    pub failed: &'static str,
    /// With {date}.
    pub credit: &'static str,
    pub text_only: &'static str,
}

pub fn text(l: Lang) -> &'static Text {
    match l {
        Lang::En => &EN,
        Lang::De => &DE,
        Lang::Fr => &FR,
        Lang::It => &IT,
    }
}

const EN: Text = Text {
    welcome: "Hello! To talk to your assistant here, open your cloud, go to Assistant → Telegram and send me the code shown there.",
    paired: "Connected. Ask your assistant anything. /new starts a new conversation, /disconnect unlinks this chat.\n\nReminder: Telegram is not end-to-end encrypted.",
    bad_code: "This code is unknown or has expired. Please create a new one in your cloud under Assistant → Telegram.",
    new_chat: "New conversation started.",
    disconnected: "Disconnected. Your assistant no longer answers here.",
    unavailable: "Your assistant is not available right now.",
    failed: "Something went wrong. Please try again.",
    credit: "Credit used up – resets on {date}.",
    text_only: "I can only read text messages.",
};

const DE: Text = Text {
    welcome: "Hallo! Um hier mit Ihrem Assistenten zu sprechen, öffnen Sie Ihre Cloud, gehen Sie zu Assistent → Telegram und senden Sie mir den angezeigten Code.",
    paired: "Verbunden. Fragen Sie Ihren Assistenten, was Sie möchten. /new startet eine neue Unterhaltung, /disconnect trennt diesen Chat.\n\nZur Erinnerung: Telegram ist nicht Ende-zu-Ende-verschlüsselt.",
    bad_code: "Dieser Code ist unbekannt oder abgelaufen. Bitte erstellen Sie in Ihrer Cloud unter Assistent → Telegram einen neuen.",
    new_chat: "Neue Unterhaltung gestartet.",
    disconnected: "Getrennt. Ihr Assistent antwortet hier nicht mehr.",
    unavailable: "Ihr Assistent ist gerade nicht verfügbar.",
    failed: "Etwas ist schiefgelaufen. Bitte versuchen Sie es nochmals.",
    credit: "Guthaben aufgebraucht – wird am {date} zurückgesetzt.",
    text_only: "Ich kann nur Textnachrichten lesen.",
};

const FR: Text = Text {
    welcome: "Bonjour ! Pour parler ici à votre assistant, ouvrez votre cloud, allez dans Assistant → Telegram et envoyez-moi le code affiché.",
    paired: "Connecté. Posez vos questions à votre assistant. /new démarre une nouvelle discussion, /disconnect déconnecte ce chat.\n\nRappel : Telegram n'est pas chiffré de bout en bout.",
    bad_code: "Ce code est inconnu ou a expiré. Veuillez en créer un nouveau dans votre cloud sous Assistant → Telegram.",
    new_chat: "Nouvelle discussion démarrée.",
    disconnected: "Déconnecté. Votre assistant ne répond plus ici.",
    unavailable: "Votre assistant n'est pas disponible pour le moment.",
    failed: "Un problème est survenu. Veuillez réessayer.",
    credit: "Crédit épuisé – renouvelé le {date}.",
    text_only: "Je ne peux lire que des messages texte.",
};

const IT: Text = Text {
    welcome: "Ciao! Per parlare qui con il tuo assistente, apri il tuo cloud, vai su Assistente → Telegram e inviami il codice mostrato.",
    paired: "Collegato. Chiedi al tuo assistente quello che vuoi. /new avvia una nuova conversazione, /disconnect scollega questa chat.\n\nPromemoria: Telegram non è crittografato end-to-end.",
    bad_code: "Questo codice è sconosciuto o scaduto. Creane uno nuovo nel tuo cloud in Assistente → Telegram.",
    new_chat: "Nuova conversazione avviata.",
    disconnected: "Scollegato. Il tuo assistente non risponde più qui.",
    unavailable: "Il tuo assistente non è disponibile al momento.",
    failed: "Qualcosa è andato storto. Riprova.",
    credit: "Credito esaurito – si rinnova il {date}.",
    text_only: "Posso leggere solo messaggi di testo.",
};

const MONTHS: [[&str; 12]; 4] = [
    [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ],
    [
        "Januar",
        "Februar",
        "März",
        "April",
        "Mai",
        "Juni",
        "Juli",
        "August",
        "September",
        "Oktober",
        "November",
        "Dezember",
    ],
    [
        "janvier",
        "février",
        "mars",
        "avril",
        "mai",
        "juin",
        "juillet",
        "août",
        "septembre",
        "octobre",
        "novembre",
        "décembre",
    ],
    [
        "gennaio",
        "febbraio",
        "marzo",
        "aprile",
        "maggio",
        "giugno",
        "luglio",
        "agosto",
        "settembre",
        "ottobre",
        "novembre",
        "dicembre",
    ],
];

/// A date as Intl.DateTimeFormat("<lang>-CH", { day, month: "long", year }) writes it.
pub fn long_date(l: Lang, y: i32, m: u32, d: u32) -> String {
    let i = match l {
        Lang::En => 0,
        Lang::De => 1,
        Lang::Fr => 2,
        Lang::It => 3,
    };
    let month = MONTHS
        .get(i)
        .and_then(|ms| ms.get(m.saturating_sub(1) as usize))
        .copied()
        .unwrap_or("");
    if l == Lang::De {
        format!("{d}. {month} {y}")
    } else {
        format!("{d} {month} {y}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_and_dates() {
        assert_eq!(lang(&[None, Some("fr-CH")]), Lang::Fr);
        assert_eq!(lang(&[Some("rm"), Some("it")]), Lang::It);
        assert_eq!(lang(&[Some("x")]), Lang::De);
        assert_eq!(long_date(Lang::De, 2026, 1, 1), "1. Januar 2026");
        assert_eq!(long_date(Lang::En, 2026, 8, 21), "21 August 2026");
        assert_eq!(long_date(Lang::Fr, 2026, 8, 21), "21 août 2026");
        assert_eq!(long_date(Lang::It, 2026, 1, 1), "1 gennaio 2026");
    }
}
