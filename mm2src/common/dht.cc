#include <chrono>
#include <cstdint>
#include <functional>
#include <iostream>
#include <random>
#include <sstream>
#include <thread>
#include <vector>
#include "libtorrent/alert_types.hpp"
#include "libtorrent/bencode.hpp"
#include "libtorrent/entry.hpp"
#include "libtorrent/kademlia/ed25519.hpp"
#include "libtorrent/kademlia/item.hpp"
#include "libtorrent/session.hpp"

extern "C" void dht_init() try {
    if (1 == 1) return;

    lt::settings_pack sett;
    sett.set_bool (lt::settings_pack::enable_dht, false);
    sett.set_int (lt::settings_pack::alert_mask, 0x7fffffff);

    sett.set_str (lt::settings_pack::dht_bootstrap_nodes,
        // https://stackoverflow.com/a/32797766/257568
        "router.utorrent.com:6881"
        ",router.bittorrent.com:6881"
        ",dht.transmissionbt.com:6881"
        ",router.bitcomet.com:6881"
        ",dht.aelitis.com:6881");

    lt::session s (sett);

    lt::dht::dht_settings dsett;
    dsett.item_lifetime = 600;
    dsett.upload_rate_limit = 64000;
    s.set_dht_settings (dsett);

    std::cout << "dht_init:" << __LINE__ << "] Enabling the DHT ..." << std::endl;
	sett.set_bool (lt::settings_pack::enable_dht, true);
	s.apply_settings (sett);

    std::cout << "dht_init:" << __LINE__ << "] Waiting for the dht_bootstrap_alert ..." << std::endl;
    for (;;) {
        std::vector<lt::alert*> alerts;
        s.pop_alerts (&alerts);
        for (lt::alert* a : alerts) {
            if (a->type() == lt::dht_bootstrap_alert::alert_type) {
                auto* dba = static_cast<lt::dht_bootstrap_alert*> (a);
                std::cout << "dht_init:" << __LINE__ << "] dht_bootstrap_alert: " << dba->message() << std::endl;
                goto bootstrapped;
            }
        }

        std::this_thread::sleep_for (std::chrono::milliseconds (100));
    }
    bootstrapped:

    std::array<char, 32> seed;
    std::random_device rd;
    std::default_random_engine rng (rd());
    std::uniform_int_distribution<std::default_random_engine::result_type> dist
        ((std::default_random_engine::result_type) CHAR_MIN, (std::default_random_engine::result_type) CHAR_MAX);
    for (char& ch : seed) ch = (char) dist (rng);
	lt::dht::public_key pk;
    lt::dht::secret_key sk;
    std::tie (pk, sk) = lt::dht::ed25519_create_keypair (seed);

    for (int i = 0; i < 55; ++i) {
        s.post_dht_stats();

        if (i < 9) {
            std::stringstream salt; salt << i;
            std::string salt_copy = salt.str();
            s.dht_put_item (
                pk.bytes,
                [salt_copy, pk, sk] (lt::entry& en, std::array<char, 64>& sig, std::int64_t& seq, std::string const&) {
                    en = "foobar";
                    std::vector<char> buf;
                    lt::bencode (std::back_inserter (buf), "foobar");
                    lt::dht::signature sign;
                    ++seq;
                    sign = lt::dht::sign_mutable_item (buf, salt_copy, lt::dht::sequence_number (seq), pk, sk);
                    sig = sign.bytes;
                },
                salt.str());
        }

        std::this_thread::sleep_for (std::chrono::seconds (1));

        if ((i > 9 && i < 19) || (i > 19 && i < 29)) {
            std::stringstream salt; salt << (i > 19 ? i - 20 : i - 10);
            s.dht_get_item (pk.bytes, salt.str());
        }

        // https://www.libtorrent.org/reference-Alerts.html
        std::vector<lt::alert*> alerts;
        s.pop_alerts (&alerts);
        for (lt::alert* a : alerts) {
            if (a->type() == lt::dht_stats_alert::alert_type) {
                auto* dsa = static_cast<lt::dht_stats_alert*> (a);
                std::cout << "dht_init:" << __LINE__ << "] dht_stats_alert: " << dsa->message() << std::endl;
            } else if (a->type() == lt::dht_put_alert::alert_type) {
                auto* dpa = static_cast<lt::dht_put_alert*> (a);
                std::cout << "dht_init:" << __LINE__ << "] dht_put_alert: " << dpa->message() << std::endl;
            } else if (a->type() == lt::dht_mutable_item_alert::alert_type) {
                auto* dmi = static_cast<lt::dht_mutable_item_alert*> (a);
                std::cout << "dht_init:" << __LINE__ << "] dht_mutable_item_alert: " << dmi->message() << "; val: " << dmi->item.to_string() << std::endl;
            }
        }
    }
} catch (std::exception const& ex) {
    std::cerr << "dht_init] ex: " << ex.what() << std::endl;
}