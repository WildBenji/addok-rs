# addok-rs

**Le géocodage des adresses françaises d'[addok](https://github.com/addok/addok), réécrit en Rust : les mêmes réponses, environ quarante fois plus vite, sans Redis ni SQLite.**

[![Licence MIT](https://img.shields.io/badge/licence-MIT-blue.svg)](LICENSE)
[![Image Docker](https://img.shields.io/badge/docker-ghcr.io%2Fwildbenji%2Faddok--rs-2496ED?logo=docker&logoColor=white)](https://github.com/WildBenji/addok-rs/pkgs/container/addok-rs)

addok-rs fait une seule chose : redresser et géocoder des adresses françaises, par lots, à partir de la [Base Adresse Nationale](https://adresse.data.gouv.fr) (BAN). Il ne cherche pas à être un moteur générique comme addok. Il reprend exactement la combinaison qu'utilise la BAN, addok et ses extensions pour la France (addok-fr, addok-france, addok-csv), et la grave dans le code : même normalisation du texte, même phonétique, même classement, mêmes scores.

Ce choix lui permet de faire ce travail mieux et beaucoup plus vite qu'addok et ses plugins ne l'ont jamais fait : un seul binaire, un index de 1,86 Go projeté en mémoire, tous les cœurs de la machine, des fichiers Parquet ou CSV en entrée comme en sortie, et des améliorations qu'addok n'offre pas (code postal faux rattrapé, numéro découpé).

```sh
docker run -d -p 7878:7878 -v "$PWD/index:/data:ro" ghcr.io/wildbenji/addok-rs
curl -F data=@adresses.csv -F columns=adresse -F columns=ville -F columns=code_postal \
     http://localhost:7878/search/csv -o geocodees.csv
```

> 📘 Installation, construction de l'index, référence de l'API et mise en production : voir le **[guide d'utilisation](HOW-TO-USE.md)**.

---

## En bref

| | addok (Python) | addok-rs |
|---|---|---|
| **Débit de bout en bout**, `/search/csv`, requêtes de 1 000 lignes | 992 adresses/s | **40 700 à 47 800 adresses/s** |
| **Géocodage d'un fichier Parquet** (100 000 lignes, lecture et écriture comprises) | impossible sans conversion en CSV | **44 745 adresses/s**, sans serveur |
| **Index** | 5,2 Go dans Redis + 2,3 Go dans SQLite | **1,86 Go**, un seul fichier |
| **Démarrage**, index prêt à répondre | 21 s à chaud, 102 s à froid | **moins d'une milliseconde** |
| **Construction de l'index, France entière** | 21 min, plafonnée par Redis | **85 s**, sur un seul cœur |
| **À faire tourner** | gunicorn, ses workers Python, Redis et SQLite | **un seul binaire** |
| **Réponses identiques à addok 1.3.2** (88 811 requêtes réelles) | — | **99,86 %**, chaque écart expliqué |

*Mesures faites sur un MacBook M1 Pro à 8 cœurs avec 100 000 adresses postales françaises réelles, saisies à la main, dans leur ordre d'origine (département par département). Le chiffre d'addok est celui de l'image `etalab/addok`. Celui d'addok-rs inclut le client HTTP, qui tournait sur la même machine.*

---

## Performances

addok donne de bonnes réponses : son classement est le fruit d'années de réglages sur les adresses françaises, et addok-rs le reprend tel quel. Ce qui le limite, c'est son architecture : Python, Redis et SQLite, pensés pour l'autocomplétion interactive plutôt que pour géocoder des millions d'adresses. Mesuré sur des adresses réelles, son temps ne part pas dans l'algorithme, mais dans les frais généraux.

| | addok | addok-rs |
|---|---|---|
| **Un cœur** | environ 105 adresses/s (un processus Python) | **9 726 adresses/s** (un thread) |
| **La machine entière** (8 cœurs) | 992 adresses/s (24 workers gunicorn, son meilleur réglage) | **45 942 adresses/s** (8 threads) |
| **Ce qui plafonne** | Redis, mono-thread : vers 1 600 adresses/s, quel que soit le nombre de cœurs | rien de partagé : le débit suit le nombre de cœurs |

*Mesures faites en processus, sans HTTP, sur un MacBook M1 Pro (6 cœurs performance, 2 cœurs efficacité), avec 100 000 adresses postales françaises réelles, saisies à la main, dans leur ordre d'origine (département par département). Le chiffre d'addok sur la machine entière est celui de l'image `etalab/addok`, mesuré de bout en bout.*

### Sur un seul cœur : environ 90 fois plus vite

Un processus addok met 9,5 ms par adresse. Presque tout ce temps part en frais généraux :

| Où passe le temps d'addok, par adresse | Part |
|---|---|
| 27 allers-retours avec Redis, chacun encodé puis décodé | 34 % |
| La comparaison de chaînes, qui reconstruit un index de n-grammes à chaque appel (93 fois par adresse) | 31 % |
| Les documents : des rues entières, avec tous leurs numéros, lues dans SQLite puis décompressées (16,7 par adresse) pour quelques champs | 26 % |
| La logique de recherche elle-même, et le traitement du texte | 8 % |

addok-rs supprime ces frais un par un. Il ne fait aucun aller-retour réseau : l'index est dans le processus. Il compte les n-grammes une seule fois, dans une table. Il lit les documents sur place, dans le fichier projeté en mémoire, et ne décode entièrement que les résultats rendus. Quant au seul vrai travail, l'intersection des listes de documents, Rust l'exécute en 13 % du temps de Redis, avec des réponses identiques sur les 405 679 intersections d'un jeu de 100 000 adresses. Il reste 0,10 ms par adresse.

### Sur plusieurs cœurs : plus de goulot d'étranglement

**addok ne peut pas utiliser plusieurs cœurs dans un même processus.** Le verrou global de l'interpréteur Python (GIL) n'y laisse avancer qu'une requête à la fois. Pour occuper la machine, on multiplie les workers gunicorn : autant de processus, chacun avec sa mémoire, ses caches et sa connexion à Redis. Et tous se partagent **un seul Redis, mono-thread**, qui devient le goulot :

- **à 992 adresses/s,** il occupe déjà 62 % de son cœur ; même avec un moteur de recherche qui ne coûterait rien, il plafonnerait vers 1 600 adresses/s ;
- **sur des adresses en ordre aléatoire,** il monte à 81 % de son cœur dès 597 adresses/s, et sa file d'attente retient les 24 workers ;
- **ajouter des workers ou des cœurs n'y change rien :** de 8 à 16 processus, addok ne gagne que 15 %.

**addok-rs n'a rien à partager.** Tous ses threads lisent le même index, en lecture seule, projeté une seule fois en mémoire : il n'y a ni verrou, ni processus à multiplier, ni serveur central. Chaque requête occupe un cœur, et le débit monte avec le nombre de cœurs :

| Threads | 1 | 2 | 4 | 6 | 8 |
|---|---|---|---|---|---|
| **Adresses/s** | 9 726 | 19 052 | 33 593 | 45 300 | 45 942 |
| **Gain sur un thread** | ×1 | ×2,0 | ×3,5 | ×4,7 | ×4,7 |

La montée est presque linéaire sur les 6 cœurs performance du M1 Pro ; les 2 cœurs efficacité n'ajoutent presque rien. Rien dans l'architecture ne fait goulot : sur une machine avec plus de cœurs, le débit devrait continuer de les suivre, ce qui reste à mesurer. Le débit de bout en bout suit : de 40 700 à 47 800 adresses/s en HTTP, et 44 745 adresses/s avec `addok-cli batch`, lecture et écriture du Parquet comprises.

---

## Les autres limites d'addok

### Redis et SQLite à exploiter

- **Tout l'index doit tenir en mémoire vive** dans Redis, et être rechargé à chaque démarrage : 21 s à chaud, 102 s à froid, pendant lesquelles le service ne répond pas.
- **Mettre à jour la BAN interrompt le service,** le temps de recharger Redis et de remplacer la base SQLite.
- **Construire l'index prend 21 minutes,** et Redis, mono-thread, est là encore le goulot : à la fin, il tourne à 100 % pendant que les workers d'import attendent.

### Des réponses qui dépendent du hasard

- **L'ordre des ex æquo est laissé au hasard :** il dépend de la graine de hachage de Python et des identifiants internes que Redis attribue à la construction. Entre deux redémarrages ou deux constructions de l'index, addok change son meilleur résultat sur 80 requêtes réelles sur 88 811.

### L'image Docker figée

- **L'image `etalab/addok` embarque addok 1.0.3, figé depuis 2022.** Elle n'a pas trois ans de corrections amont : 66 types de voie reconnus au lieu d'environ 38, libellés des communes fusionnées, règles phonétiques réécrites.
- **L'archive de la BAN prête à l'emploi est indexée avec les anciennes règles phonétiques.** On ne peut donc pas l'utiliser avec un addok à jour sans reconstruire l'index : 21 minutes.

### Les défauts d'addok-csv

- **Le séparateur est parfois mal deviné :** une adresse comme `2 RUE L 'EST ' B` lui fait prendre l'espace pour séparateur. La requête est alors refusée avec ses 1 000 lignes, ce qui arrive à environ une requête sur cent sur des données réelles.
- **Une seule ligne de plus de 200 caractères fait refuser tout le fichier,** avec l'erreur 413.
- **La colonne `result_street` est toujours vide.**
- **Il ne connaît que le CSV :** tout est converti en texte, à l'aller comme au retour.
- **Ses filtres ne fonctionnent plus** avec addok 1.3.2 : addok-csv 1.1.0 échoue dès qu'on lui en passe un.

---

## Ce qu'apporte addok-rs

### Performances

- **Environ 90 fois plus vite sur un cœur, et 46 fois plus sur la machine entière,** sans goulot partagé ([ci-dessus](#performances)).

### Exploitation

- **Un seul binaire, un seul fichier d'index.** Il n'y a ni Redis ni SQLite à dimensionner, à superviser ou à sauvegarder.
- **Un démarrage instantané :** l'index est projeté en mémoire, pas chargé.
- **Une mise à jour de la BAN sans interruption.** Le nouvel index est construit à côté de l'ancien en 85 s, puis le remplace d'un coup. Le serveur continue de répondre pendant la construction, et son redémarrage est instantané.
- **Une image Docker publique,** pour amd64 et arm64, qui ne contient que le binaire. L'index est monté de l'extérieur : une nouvelle édition de la BAN, c'est un nouveau fichier, pas une nouvelle image.
- **`GET /health`** répond tout de suite, même sous charge, avec le nombre de documents de l'index chargé et la version.

### Interfaces

- **`POST /search/csv` reproduit addok-csv à l'octet près,** moins ses trois défauts. Un client d'addok existant passe à addok-rs en changeant seulement l'URL.
- **`POST /batch` prend du Parquet ou du CSV et rend du Parquet ou du CSV.** Les colonnes d'origine gardent leur type, et les résultats sont typés : scores et coordonnées en nombres, `null` en l'absence de résultat.
- **`addok-cli batch` géocode un fichier entier sans serveur,** sur tous les cœurs.
- **Les filtres d'addok** (`type`, `citycode`, `postcode`) sur les trois interfaces : chaque ligne est filtrée par la valeur de ses propres colonnes, ce qu'addok-csv 1.1.0 ne sait plus faire. Ils donnent les réponses d'addok 1.3.2 sous les mêmes filtres : 99,87 % de réponses identiques sur 540 226 recherches filtrées, chaque écart expliqué.
- **Une ligne invalide n'est plus fatale.** Une adresse trop longue reste seule sans résultat, et un en-tête `X-Addok-Warning` dit laquelle.

### Des réponses reproductibles

- **Les ex æquo sont départagés par une règle explicite et documentée :** le score, puis l'importance, puis l'identifiant BAN. Avec le même index, la même requête donne toujours la même réponse, d'un redémarrage à l'autre et d'une machine à l'autre.
- **Les règles sont celles d'addok 1.3.2,** la version amont la plus récente, et non celles de l'image de 2022.

### Mieux qu'addok, à la demande

Chaque amélioration est désactivée par défaut, afin qu'addok-rs réponde comme addok tant qu'on ne demande rien d'autre. Chacune a été mesurée sur des adresses réelles.

- **Le repli sur la commune quand le code postal est faux,** avec `postcode_fallback=1`. `4 RUE CLEMENT MAROT, PERPIGNAN, 66100` rend le 4 rue Clément Marot à Perpignan (66000), là où addok répond une autre rue sous le seuil. Sur 648 328 adresses réelles, il fait trouver plus de 2 000 adresses justes de plus, pour la même part de réponses justes.
- **Le numéro découpé,** avec `result_columns` : `result_num` (`12`), `result_num_complement` tel que la BAN l'écrit (`bis`), et sa forme courte (`b`) pour rapprocher d'autres données.

---

## Les mêmes réponses qu'addok

Les seuils de confiance de ceux qui utilisent addok sont réglés sur son échelle de scores : un classement « meilleur » qui décalerait les scores changerait en silence les adresses retenues. addok-rs donne donc d'abord les réponses d'addok, scores compris. Il a été comparé à la pile Python complète (addok 1.3.2, addok-fr 1.1.0, addok-france 1.2.0, addok-csv 1.1.0) sur des adresses réelles, saisies à la main.

Pour chaque requête, deux cas comptent comme une correspondance :

- **identique** : le même résultat, avec le même score au dernier chiffre près ;
- **ex æquo** : un autre résultat qui a exactement le même score.

Tout le reste est un **écart**, et chaque écart doit être expliqué. Deux ordres que la pile Python laisse au hasard les expliquent tous : le départage des ex æquo par Redis, et l'ordre dans lequel Python regroupe les mots. Dans la plupart des cas (61 écarts sur 84 pour les adresses complètes), donner à addok-rs le même ordre lui fait redonner exactement la réponse d'addok.

| Jeu de requêtes (adresses réelles) | Requêtes | Identiques | Ex æquo | Écarts, tous expliqués |
|---|---|---|---|---|
| Adresses complètes | 88 811 | 88 690 (99,86 %) | 37 | 84 |
| Sans code postal | 88 466 | 88 374 (99,90 %) | 39 | 53 |
| Sans ville | 88 067 | 87 999 (99,92 %) | 34 | 34 |

- **Le deuxième résultat** est vérifié de la même façon. addok-csv en publie le score dans `result_score_next`, et là encore, chaque écart est expliqué.
- **La sortie CSV** de `/search/csv` a été comparée octet par octet aux réponses d'addok-csv sur 100 requêtes de 1 000 lignes. Les seules différences qui ne viennent pas de la recherche sont deux des défauts corrigés.
- **addok n'est pas toujours d'accord avec lui-même.** D'une graine de hachage ou d'une construction de l'index à l'autre, il change son meilleur résultat sur 80 de ces requêtes, et son deuxième sur 210 : aucune réimplémentation ne peut faire mieux que ce seuil.

---

## Démarrage rapide

### Avec Docker

```sh
# 1. Télécharger l'export Addok de la BAN (1,3 Go, republié chaque nuit)
mkdir -p index && cd index
curl -LO https://adresse.data.gouv.fr/data/ban/adresses/latest/addok/adresses-addok-france.ndjson.gz

# 2. Construire l'index (environ 1 min 30)
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/data" --entrypoint addok-cli \
  ghcr.io/wildbenji/addok-rs build /data/adresses-addok-france.ndjson.gz /data/ban.addok

# 3. Servir l'index
docker run -d --name addok-rs -p 7878:7878 -v "$PWD:/data:ro" ghcr.io/wildbenji/addok-rs
curl http://localhost:7878/health
```

### Depuis les sources

```sh
cargo build --release
target/release/addok-cli build adresses-addok-france.ndjson.gz ban.addok

# Géocoder un fichier, sans serveur
target/release/addok-cli batch ban.addok adresses.parquet geocodees.parquet --columns adresse,ville,code_postal

# Ou servir l'index en HTTP
target/release/addok-cli serve ban.addok --host 0.0.0.0
```

Tout le reste est dans le **[guide d'utilisation](HOW-TO-USE.md)** : paramètres, exemples de clients, colonnes de résultat, mise à jour de la BAN, mise en production et dépannage.

---

## Limites

addok-rs est un géocodeur **par lots, pour les adresses françaises de la BAN**, et l'assume :

- **Ce n'est pas un géocodeur généraliste.** addok assemble son pipeline à partir de plugins déclarés dans sa configuration ; addok-rs écrit en dur la seule combinaison que la BAN utilise : phonétique et synonymes français, types de voie, compléments `bis` et `ter`, clavier AZERTY pour les fautes de frappe, schéma des documents de la BAN. Une seule combinaison, c'est ce qui permet de vérifier chaque réponse face à addok et de garder le code direct. Indexer une autre source ou un autre pays demanderait de changer le code, pas la configuration.
- **Ces fonctions d'addok ne sont pas encore portées** :
  - la **recherche autour d'un point** (`lat`, `lon`) et le **géocodage inverse** ;
  - le point d'entrée JSON `/search`, et l'autocomplétion.
- **Pas d'authentification,** comme addok : le serveur est fait pour un réseau interne.

---

## Organisation du dépôt

```
crates/
  addok-core/   le moteur, sans entrées-sorties : chaîne de texte, index, recherche
  addok-cli/    la ligne de commande : build, serve, batch ; HTTP, CSV et Parquet
```

- **`addok-core`** ne fait aucune entrée-sortie. Il porte la chaîne de traitement du texte d'addok, l'index (un fichier de sections lu sur place, sans désérialisation) et la recherche.
- **`addok-cli`** y ajoute le serveur HTTP, une transcription du module `csv` de Python (pour reproduire addok-csv à l'octet près, défauts corrigés exceptés) et la lecture et l'écriture de Parquet.

---

## Développer

```sh
cargo build --release                                    # le binaire : target/release/addok-cli
cargo test --workspace                                   # tests unitaires, en quelques secondes
cargo clippy --workspace --all-targets -- -D warnings    # aucun avertissement toléré
```

---

## Pour aller plus loin

- **[HOW-TO-USE.md](HOW-TO-USE.md)** : le guide d'utilisation.
- **[CHANGELOG.md](CHANGELOG.md)** : l'historique des versions.

## Licence

Le code d'addok-rs est publié sous [licence MIT](LICENSE), © 2026 Klarsen. La chaîne de traitement du texte est portée d'addok, addok-fr et addok-france, et `synonyms.txt` est repris d'addok-fr, tous sous licence MIT, © DINUM/Etalab ([`LICENSE-addok`](crates/addok-core/src/text/LICENSE-addok)).

Données : © Base Adresse Nationale, sous [Licence Ouverte 2.0](https://www.etalab.gouv.fr/licence-ouverte-open-licence/).
