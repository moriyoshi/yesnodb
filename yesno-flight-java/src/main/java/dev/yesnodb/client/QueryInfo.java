package dev.yesnodb.client;

import java.util.List;
import java.util.Objects;
import org.apache.arrow.flight.FlightEndpoint;
import org.apache.arrow.flight.FlightInfo;
import org.apache.arrow.flight.Ticket;

/** Exact result metadata and the versioned ticket that produced it. */
public final class QueryInfo {
  private final FlightInfo flightInfo;
  private final Ticket flightTicket;
  private final QueryTicket ticket;
  private final long totalRecords;

  private QueryInfo(
      FlightInfo flightInfo, Ticket flightTicket, QueryTicket ticket, long totalRecords) {
    this.flightInfo = flightInfo;
    this.flightTicket = flightTicket;
    this.ticket = ticket;
    this.totalRecords = totalRecords;
  }

  static QueryInfo from(FlightInfo flightInfo) {
    Objects.requireNonNull(flightInfo, "flightInfo");
    if (flightInfo.getRecords() < 0) {
      throw new IllegalStateException(
          "yesnodb returned a negative total_records value: " + flightInfo.getRecords());
    }
    List<FlightEndpoint> endpoints = flightInfo.getEndpoints();
    if (endpoints.size() != 1) {
      throw new IllegalStateException(
          "yesnodb returned " + endpoints.size() + " endpoints instead of 1");
    }
    Ticket flightTicket = endpoints.get(0).getTicket();
    byte[] bytes = flightTicket.getBytes().clone();
    QueryTicket ticket;
    try {
      ticket = QueryTicket.decode(bytes);
    } catch (IllegalArgumentException exception) {
      throw new IllegalStateException("yesnodb returned a malformed query ticket", exception);
    }
    return new QueryInfo(flightInfo, new Ticket(bytes), ticket, flightInfo.getRecords());
  }

  /** The exact number of ordinals in this result. */
  public long totalRecords() {
    return totalRecords;
  }

  /** The database version shared by the count and eventual row stream. */
  public long version() {
    return ticket.version();
  }

  /** The decoded yesnodb ticket. */
  public QueryTicket ticket() {
    return ticket;
  }

  /** A defensive copy of the opaque Flight ticket bytes. */
  public byte[] ticketBytes() {
    return flightTicket.getBytes().clone();
  }

  /** The complete Flight metadata returned by the server. */
  public FlightInfo flightInfo() {
    return flightInfo;
  }

  Ticket flightTicket() {
    return flightTicket;
  }
}
