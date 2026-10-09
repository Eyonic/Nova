/*! NOVA demo cart | MIT */
// A small, readable script as a developer would write it. NOVA's Script
// Optimizer minifies and precompresses it in the background; the source
// file on disk is never modified.
(function () {
    'use strict';

    var storageKey = 'nova-demo-cart';

    function readCart() {
        try {
            return JSON.parse(window.localStorage.getItem(storageKey)) || [];
        } catch (error) {
            return [];
        }
    }

    function writeCart(items) {
        window.localStorage.setItem(storageKey, JSON.stringify(items));
    }

    function addToCart(productIdentifier, requestedQuantity) {
        var items = readCart();
        items.push({ id: productIdentifier, quantity: requestedQuantity || 1 });
        writeCart(items);
        return items.length;
    }

    window.novaCart = { add: addToCart, items: readCart };
})();
